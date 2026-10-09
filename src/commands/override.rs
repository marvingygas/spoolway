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
    if repo.in_linked_worktree() {
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

/// How wide [`IgnoredPopup::panel`] wraps a row at most: wider than
/// [`crate::screen::NOTICE_WRAP`], because a validation error's reason is
/// one sentence with its own trailing explanation — the Mockup draws
/// `names both `run:` and `agent:` — a step runs a process or a model, not
/// both` on one row — and the box it makes still fits inside the 100
/// columns every tab is drawn to.
pub(crate) const IGNORED_POPUP_WRAP: usize = 80;

/// The narrowest the keys column is ever wrapped to. A file and a step
/// wider than the whole popup would otherwise leave it no room at all, and
/// [`crate::screen::wrap`] would stack the keys one character to a row.
const IGNORED_KEYS_MIN: usize = 12;

/// The "override ignored" popup bare `spoolway` opens on — see
/// [`crate::screen::shell::OnOpen`] and [`ignored_popup`]. It holds the
/// entries rather than a drawn panel, so the tab under it can wrap them to
/// the width its own frame has on the draw that shows them.
#[derive(Debug, Clone)]
pub(crate) struct IgnoredPopup {
    entries: Vec<IgnoredEntry>,
}

/// One ignored override, as [`IgnoredPopup`] draws it.
#[derive(Debug, Clone)]
struct IgnoredEntry {
    file: String,
    step: String,
    keys: String,
    reason: String,
}

/// The "override ignored" popup, or `None` with nothing in the layer left
/// out of the merge.
///
/// Asked on every open and never acknowledged, unlike the before-start
/// overrides popup's `[x]`: a skipped override changes what lanes run, and
/// a person who silenced that popup for an older layer would otherwise
/// start dispatching without ever learning an entry stopped fitting.
pub(crate) fn ignored_popup(repo: &Repo) -> Result<Option<IgnoredPopup>> {
    Ok(IgnoredPopup::from_rows(&collect_override_rows(repo)?))
}

impl IgnoredPopup {
    /// Every ignored entry across `rows`, or `None` with none — `rows` as
    /// [`collect_override_rows`] returns them.
    pub(crate) fn from_rows(rows: &[OverrideRow]) -> Option<IgnoredPopup> {
        let entries: Vec<IgnoredEntry> = rows
            .iter()
            .flat_map(|row| {
                row.ignored.iter().map(move |item| {
                    let (step, keys) = ignored_columns(row, item);
                    IgnoredEntry {
                        file: row.target.clone(),
                        step,
                        keys,
                        reason: item.reason.clone(),
                    }
                })
            })
            .collect();
        (!entries.is_empty()).then_some(IgnoredPopup { entries })
    }

    /// The boxed popup, its rows wrapped to `width`, over
    /// [`crate::screen::confirm`]'s `[enter] confirm`.
    pub(crate) fn panel(&self, width: usize) -> Vec<String> {
        let mut body = vec![String::new()];
        body.extend(self.lines(width));
        crate::screen::panel("override ignored", &body, &crate::screen::confirm())
    }

    /// One pair of rows per ignored override: the file, the step and the
    /// keys it sets, then the full reason under them — the whole reason
    /// rather than [`crate::overrides::Ignored::short_reason`], since this
    /// popup is the one place with the room to say why.
    fn lines(&self, width: usize) -> Vec<String> {
        let file_w = self.entries.iter().map(|e| e.file.len()).max().unwrap_or(0);
        let step_w = self
            .entries
            .iter()
            .map(|e| e.step.chars().count())
            .max()
            .unwrap_or(0);

        let mut lines = Vec::new();
        for IgnoredEntry {
            file,
            step,
            keys,
            reason,
        } in &self.entries
        {
            // A config key or a whole file has no step to name; the column
            // is left out altogether when no entry has one, rather than
            // drawn as a run of blanks between the file and its keys.
            let head = match step_w {
                0 => format!("{file:<file_w$}   "),
                _ => format!("{file:<file_w$}   {step:<step_w$}   "),
            };
            // `wrap` collapses runs of spaces, so only the keys go through
            // it — the columns before them are padded by hand — and a long
            // list of keys continues under its own column rather than under
            // the file.
            let lead = head.chars().count();
            let room = width.saturating_sub(lead).max(IGNORED_KEYS_MIN);
            for (i, part) in crate::screen::wrap(keys, room).into_iter().enumerate() {
                match i {
                    0 => lines.push(format!("{head}{part}")),
                    _ => lines.push(format!("{}{part}", " ".repeat(lead))),
                }
            }
            lines.extend(crate::screen::wrap(&format!("  {reason}"), width));
        }
        lines
    }
}

/// The step and keys columns for one ignored entry: `step publish` and
/// `agent, model` for a pipeline step's entry — its `fields` are dotted
/// under the step, `publish.agent, publish.model` — no step and the dotted
/// key itself for a config entry, and `the whole file` for an entry that
/// names no field at all, the same words `override list` uses for it.
/// Shared with `commands::dispatch`'s before-start overrides popup, which
/// labels its own `ignored` row the same way.
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

    /// The Mockup's "override ignored" popup: the file, the step and the
    /// keys it sets on one row, the whole reason under it — and no popup
    /// at all once nothing in the layer is ignored.
    #[test]
    fn ignored_popup_names_each_ignored_override_with_its_whole_reason() {
        with_repo("ignored-popup", |repo| {
            assert!(ignored_popup(repo).unwrap().is_none(), "no layer at all");

            let dir = repo.overrides_dir();
            write_atomic(
                &crate::overrides::pipeline_patch_path(&dir, "demo"),
                "steps:\n  implement:\n    run: echo hi\n    model: claude-opus-5\n",
            )
            .unwrap();

            let panel = ignored_popup(repo)
                .unwrap()
                .expect("one override is ignored")
                .panel(IGNORED_POPUP_WRAP);
            let drawn = panel.join("\n");
            assert!(panel[0].starts_with("┌─ override ignored "), "{drawn}");
            assert!(
                drawn.contains("  pipelines/demo.yml   step implement   run, model "),
                "{drawn}"
            );
            assert!(
                drawn.contains(
                    "    names both `run:` and `agent:` — a step runs a process or a model, not \
                     both"
                ),
                "{drawn}"
            );
            assert!(drawn.contains("  [enter] confirm "), "{drawn}");

            std::fs::remove_file(crate::overrides::pipeline_patch_path(&dir, "demo")).unwrap();
            assert!(ignored_popup(repo).unwrap().is_none(), "nothing ignored");
        });
    }

    /// A config key names no step, so the step column is left out; an entry
    /// with no field of its own — a prompt the checkout no longer has —
    /// reads `the whole file`, the way `override list` says it.
    #[test]
    fn ignored_lines_name_a_config_key_and_a_whole_file() {
        let rows = vec![
            OverrideRow {
                target: "prompts/retired".into(),
                kind: "whole file",
                overrides: "—".into(),
                ignored: vec![crate::overrides::Ignored::missing_prompt("retired")],
            },
            OverrideRow {
                target: crate::config::CONFIG_FILE.into(),
                kind: "patch",
                overrides: String::new(),
                ignored: vec![crate::overrides::Ignored {
                    target: crate::config::CONFIG_FILE.into(),
                    fields: "dispatch.nonesuch".into(),
                    reason: "unknown key `dispatch.nonesuch`".into(),
                }],
            },
        ];
        assert_eq!(
            IgnoredPopup::from_rows(&rows)
                .unwrap()
                .lines(IGNORED_POPUP_WRAP),
            vec![
                "prompts/retired   the whole file",
                "  names prompt `retired`, which the checkout does not have",
                "config.toml       dispatch.nonesuch",
                "  unknown key `dispatch.nonesuch`",
            ]
        );
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
                "[unattended]\nblocked_agent = \"promoted\"\n",
            )
            .unwrap();

            override_promote(repo, "config.toml").unwrap();

            let tracked_config = std::fs::read_to_string(Config::path_in(&repo.root)).unwrap();
            assert!(tracked_config.contains("blocked_agent = \"promoted\""));
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
