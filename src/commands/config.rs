//! `spoolway config`: reading and editing `config.toml` through the same
//! deserialiser that loads it.

use super::*;

/// Reads the checkout's own file — see [`config_get`] for why.
pub fn config_show(repo: &Repo, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    let config = Config::load(&repo.checkout)?;
    print!("{}", toml::to_string_pretty(&config)?);
    Ok(())
}

/// Every scalar setting as `key = value`, one per line — the same keys
/// [`config_get`] and [`config_set`] resolve, so a person can see what is
/// there to change without paging through [`config_show`]'s TOML. Reads
/// `repo.checkout` for the same reason [`config_get`] does.
///
/// The list is [`crate::confkv::all_settings`], not `entries`: it also names
/// the keys the file omits while they hold their default — the three flat
/// `[dispatch]` ones, every profile's `concurrency`, and every `[models]`
/// field of a glob the config already carries — so nothing that resolves to
/// a value today is missing from it. A `[models]` glob nobody has named yet
/// is settable but unlistable, since that keyspace has no bound.
///
/// The rows come out in key order, so a setting is where a person looks for
/// it rather than wherever the file happened to put it.
pub fn config_list(repo: &Repo, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    let config = Config::load(&repo.checkout)?;
    let entries = crate::confkv::all_settings(&config)?;

    if json {
        let rows: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| serde_json::json!({ "key": e.key, "value": e.value }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    print!("{}", render_config_list(&entries));
    Ok(())
}

/// The `key = value` block [`config_list`] prints, built as a string so a
/// test can assert on it — one row per setting, the `=` aligned in a column.
fn render_config_list(entries: &[crate::confkv::Entry]) -> String {
    let width = entries.iter().map(|e| e.key.len()).max().unwrap_or(0);
    let mut out = String::new();
    for entry in entries {
        out.push_str(&format!("{:<width$} = {}\n", entry.key, entry.value));
    }
    out
}

/// Reads `repo.checkout`, not `repo.root`: a lane standing in a linked
/// worktree is asking about the file beside it, and `repo.config` (loaded
/// from `root` once, in `Repo::discover`) would answer for a file that is not
/// the one in front of the command. In the main checkout the two are the same
/// directory, so this changes nothing there.
pub fn config_get(repo: &Repo, key: &str, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    let config = Config::load(&repo.checkout)?;
    println!("{}", crate::confkv::get(&config, key)?);
    Ok(())
}

/// Write one key into the project's own config.
///
/// Refuses inside a linked worktree. The dispatcher only ever reads
/// `repo.root`'s `config.toml`, never a worktree's own copy — so a write here
/// would sit in a file nothing reads until the branch merges, silently. `-C`
/// is the way around it, already built in, so the refusal names the exact
/// invocation that lands in the file the dispatcher actually reads. In the
/// main checkout `checkout` and `root` are the same directory, so nothing
/// here changes for it.
pub fn config_set(repo: &Repo, key: &str, value: &str) -> Result<()> {
    if repo.checkout != repo.root {
        bail!(
            "the dispatcher reads the project's config, not this worktree's.\n  spoolway -C {} \
             config set {key} {value}",
            repo.root.display()
        );
    }
    let updated = crate::confkv::set(&repo.config, key, value)?;
    updated.save_key(&repo.root, key)?;
    println!("{key} = {}", crate::confkv::get(&updated, key)?);
    Ok(())
}

/// Open the config file in the user's editor, then re-validate it.
///
/// Runs on a lenient discovery, because a config that no longer parses is
/// exactly when you want to open it. The validation afterwards uses the same
/// deserialiser every command loads through, so what this accepts is what the
/// next command will.
///
/// Takes `checkout`, not `root`, and deliberately never refuses the way
/// `config_set` does: opening the file in front of you is the point, and a
/// worktree changing this file — a task renaming a table, say — has to be
/// able to validate its own copy without reaching for `-C`.
pub fn config_edit(checkout: &Path) -> Result<()> {
    let path = Config::path_in(checkout);
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad" } else { "vi" }.to_string());

    let status = crate::platform::shell_command(&editor_command(&editor, &path))
        .status()
        .with_context(|| format!("could not start `{editor}`"))?;
    if !status.success() {
        bail!("`{editor}` exited with {status}");
    }

    match Config::load(checkout) {
        Ok(_) => {
            println!("{} — parses", path.display());
            Ok(())
        }
        Err(err) => Err(err.context(format!(
            "{} no longer parses — reopen it with `spoolway config edit`",
            path.display()
        ))),
    }
}

/// The shell line that opens `path` in `editor`.
///
/// `$VISUAL`/`$EDITOR` is interpolated unquoted on purpose, so `code --wait`
/// still splits into a program and a flag. The path is quoted through the
/// platform's own escaper, which escapes an embedded `'` for both dialects — a
/// plain `format!("… '{}'")` would break the quoting on a checkout path that
/// contains one, and the editor would exit on a syntax error (finding 71).
fn editor_command(editor: &str, path: &Path) -> String {
    format!(
        "{editor} {}",
        crate::platform::Shell::CURRENT.quote(&path.display().to_string())
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn editor_command_quotes_a_path_with_a_single_quote() {
        let line = editor_command("vi", Path::new("/home/u/it's/proj/.spoolway/config.toml"));
        // The `'` inside the path is escaped, not left to close the quoting.
        assert!(
            line.contains(r"it'\''s") || line.contains("it''s"),
            "unescaped quote in {line:?}"
        );
        // The editor is still its own unquoted token.
        assert!(line.starts_with("vi "));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::repo::run(dir, "git", args)
            .unwrap_or_else(|e| panic!("git {args:?} in {dir:?}: {e:#}"))
    }

    /// A project on its own branch, plus a linked worktree of it — real git,
    /// like `repo::tests::fixture` — so `checkout` and `root` genuinely land
    /// in two different directories, which is the one thing a fixture built
    /// by hand (`testutil::fixture`) can never give this test.
    ///
    /// Both `.spoolway/config.toml` files are written with a value the other
    /// one does not have, so a test reading the wrong file is caught by a
    /// wrong answer rather than by both files agreeing.
    fn worktree_fixture(name: &str) -> (Repo, PathBuf) {
        // A base directory unique to this call, holding both the project and
        // its worktree — mirroring `repo::tests::fixture` — so two `cargo
        // test` processes running this test at once never share a directory.
        // See `scratch`'s module doc for why that matters.
        let base = crate::scratch::root(&format!("config-checkout-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "plan/demo"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "t"]);

        let state = root.join(crate::config::STATE_DIR);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("config.toml"),
            "[dispatch]\ndefault_pipeline = \"from-root\"\n",
        )
        .unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "root"]);

        let wt = base.join("worktree");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/x",
                wt.to_str().unwrap(),
            ],
        );
        std::fs::write(
            wt.join(crate::config::STATE_DIR).join("config.toml"),
            "[dispatch]\ndefault_pipeline = \"from-worktree\"\n",
        )
        .unwrap();

        let repo = crate::repo::Repo::discover(&wt).unwrap();
        assert_ne!(
            repo.checkout, repo.root,
            "the fixture is only useful if it actually lands in a linked worktree"
        );
        (repo, root)
    }

    #[test]
    fn config_get_answers_for_the_checkout_not_the_root() {
        let (repo, _root) = worktree_fixture("get");
        config_get(&repo, "dispatch.default_pipeline", false).unwrap();
        // `config_get` prints to stdout, which a unit test cannot capture
        // cheaply — so this also asserts the lower-level read it goes
        // through, which is the part that actually decides the answer.
        let config = Config::load(&repo.checkout).unwrap();
        assert_eq!(config.dispatch.default_pipeline, "from-worktree");
    }

    #[test]
    fn config_path_names_the_worktrees_own_file() {
        let (repo, _root) = worktree_fixture("path");
        assert_eq!(
            Config::path_in(&repo.checkout),
            repo.checkout.join(".spoolway/config.toml")
        );
    }

    #[test]
    fn config_set_refuses_inside_a_linked_worktree_and_writes_nothing() {
        let (repo, root) = worktree_fixture("set");
        let before_root = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        let before_wt = std::fs::read_to_string(Config::path_in(&repo.checkout)).unwrap();

        let err = config_set(&repo, "dispatch.default_pipeline", "changed")
            .expect_err("config set must refuse inside a linked worktree");
        let message = format!("{err:#}");
        assert!(
            message.starts_with("the dispatcher reads the project's config, not this worktree's."),
            "unexpected message: {message}"
        );
        // `repo.root`'s own spelling, not the fixture's: on Windows the
        // discovery reads the root out of git, which prints forward slashes,
        // while the fixture built `root` with the platform's own — the same
        // directory as a `Path`, but not the same string.
        assert!(
            message.contains(&format!(
                "spoolway -C {} config set dispatch.default_pipeline changed",
                repo.root.display()
            )),
            "message did not name the -C invocation: {message}"
        );

        assert_eq!(
            std::fs::read_to_string(Config::path_in(&root)).unwrap(),
            before_root,
            "the project's config must be untouched by a refused set"
        );
        assert_eq!(
            std::fs::read_to_string(Config::path_in(&repo.checkout)).unwrap(),
            before_wt,
            "the worktree's own config must be untouched too — set refuses, it does not redirect"
        );
    }

    #[test]
    fn config_set_in_the_main_checkout_writes_exactly_where_it_writes_today() {
        let repo = crate::commands::testutil::fixture("config-set-main");
        config_set(&repo, "dispatch.default_pipeline", "changed").unwrap();
        let config = Config::load(&repo.root).unwrap();
        assert_eq!(config.dispatch.default_pipeline, "changed");
    }

    /// `config list` runs both ways, prints `key = value` rows, and names
    /// every scalar key that resolves to a value today —
    /// `issue_tracking.key_in_names` (the criterion the feature has to meet),
    /// the flat keys the file omits while they hold their default, a profile's
    /// omitted `concurrency`, and a `[models]` glob's omitted fields — all of
    /// it in key order.
    #[test]
    fn config_list_prints_every_settable_key_as_key_equals_value() {
        let repo = crate::commands::testutil::fixture("config-list");
        config_list(&repo, false).unwrap();
        config_list(&repo, true).unwrap();

        let config = Config::load(&repo.checkout).unwrap();
        // A glob nothing has named, given one field, so its other fields are
        // the per-entry omissions `config list` now has to carry.
        let config = crate::confkv::set(&config, "models.demo-model-*.input", "3.0").unwrap();
        let entries = crate::confkv::all_settings(&config).unwrap();
        let text = render_config_list(&entries);

        assert!(
            text.lines()
                .any(|l| l.starts_with("issue_tracking.key_in_names ") && l.ends_with(" = false")),
            "key_in_names row missing or not `key = value`:\n{text}"
        );
        // The `=` delimiter the help promises, not a bare column gap — and a
        // single-token key to the left of it.
        for line in text.lines() {
            let (key, _value) = line
                .split_once(" = ")
                .unwrap_or_else(|| panic!("row is not `key = value`: {line}"));
            let key = key.trim_end();
            assert!(
                !key.is_empty() && !key.contains(' '),
                "bad key in row: {line}"
            );
        }
        // Every omitted-default key `get`/`set` accepts is listed too.
        let expected = crate::confkv::OMITTED_DEFAULT_KEYS
            .iter()
            .map(|k| (*k).to_string())
            .chain([
                // A profile that omits `concurrency` — `pi` among the shipped
                // ones — and an omitted `[models]` field of the glob above.
                "agents.pi.concurrency".to_string(),
                "models.demo-model-*.output".to_string(),
                "models.demo-model-*.context_window".to_string(),
            ]);
        for key in expected {
            assert!(
                entries.iter().any(|e| e.key == key),
                "`{key}` is settable but missing from `config list`"
            );
            // And it still resolves through `get`, i.e. the value shown is real.
            assert!(crate::confkv::get(&config, &key).is_ok());
        }
        // The field that *was* set is a plain listed row, not duplicated.
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.key == "models.demo-model-*.input")
                .count(),
            1
        );
        // The omitted keys are found after the file's own, so without a sort
        // they trail the whole list. `agents.pi.concurrency` has to read
        // beside the rest of `agents.pi`, not at the bottom under `update.`.
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "`config list` is not in key order:\n{text}");
    }
}
