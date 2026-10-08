//! `spoolway config`: reading and editing `config.toml` through the same
//! deserialiser that loads it.

use std::path::PathBuf;

use serde::Serialize;

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
/// Refuses inside a linked worktree, including one that carries no
/// `.spoolway/` and reads the main checkout's. The dispatcher only ever reads
/// `repo.root`'s `config.toml`, never a worktree's own copy — so a write here
/// would sit in a file nothing reads until the branch merges, silently. `-C`
/// is the way around it, already built in, so the refusal names the exact
/// invocation that lands in the file the dispatcher actually reads. In the
/// main checkout `checkout` and `root` are the same directory, so nothing
/// here changes for it.
pub fn config_set(repo: &Repo, key: &str, value: &str) -> Result<()> {
    if repo.in_linked_worktree() {
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

/// `spoolway config path`: every place this project's own setup lives — the
/// setup folder (`.spoolway/` in repo mode, a workspace's `config/` in home
/// mode), the private layer's `local/` (repo mode only — home mode has none,
/// since the whole setup is already private), the override layer's
/// `overrides/`, the routines folder and both cron-job stores — plus this
/// checkout's own workspace and every workspace on the machine, so the
/// skill can propose `spoolway init --workspace <name>` without building a
/// path by hand.
///
/// This is what `spoolway-config`'s "never reach past" rule names, and the
/// skill reads every path from here instead of building them itself, so a
/// project laid out differently from the skill's own assumptions still
/// routes correctly. See [`config_path_anywhere`] for the checkout-no-one-
/// claims case, which this never sees — it is handed an already-discovered
/// `Repo`.
pub fn config_path(repo: &Repo, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    print_config_paths(&ConfigPaths::for_repo(repo), json)
}

/// [`config_path`]'s own entry point from `main`, for a checkout
/// `Repo::discover` may or may not recognise as a project at all —
/// `config path` is the one command the skill needs to still answer in a
/// checkout nothing claims yet, with `mode: null` and the workspace list,
/// since that is exactly the information `spoolway init --workspace <name>`
/// needs next. [`Repo::is_unclaimed`] covers both shapes that refusal takes
/// — nothing anywhere names this checkout, or a broken workspace file
/// elsewhere leaves that undecided — and in the second shape the broken
/// file still turns up in the workspace list, with its own `error` field,
/// rather than taking down the whole command. Every other refusal
/// `Repo::discover` can give — a `.spoolway/` left on another branch, a
/// workspace listing a clone whose folder is gone, two workspaces both
/// claiming this root — still names an actual owner with an actual problem,
/// so those propagate exactly as `Repo::discover` reported them.
pub fn config_path_anywhere(cwd: &Path, json: bool) -> Result<()> {
    match Repo::discover(cwd) {
        Ok(repo) => config_path(&repo, json),
        Err(err) if Repo::is_unclaimed(&err) => print_config_paths(&ConfigPaths::unclaimed(), json),
        Err(err) => Err(err),
    }
}

/// [`ConfigPaths`]'s text form — one row per field, the same order `--json`
/// lists them in, and the same wording whether or not a project was found.
fn print_config_paths(paths: &ConfigPaths, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(paths)?);
        return Ok(());
    }

    println!("mode:      {}", paths.mode.unwrap_or("-"));
    println!("setup:     {}", display_opt(paths.setup.as_deref()));
    if let Some(local) = &paths.local {
        println!("local:     {}", local.display());
    }
    println!("overrides: {}", display_opt(paths.overrides.as_deref()));
    println!("routines:  {}", display_opt(paths.routines.as_deref()));
    match &paths.jobs {
        Some(jobs) => {
            println!("jobs.user:    {}", jobs.user.display());
            println!("jobs.project: {}", jobs.project.display());
        }
        None => println!("jobs:      -"),
    }
    println!("workspace: {}", paths.workspace.as_deref().unwrap_or("-"));
    if paths.workspaces.is_empty() {
        println!("workspaces: -");
    } else {
        for place in &paths.workspaces {
            match &place.error {
                Some(error) => println!("workspaces: {} — {error}", place.name),
                None => println!(
                    "workspaces: {} {} (clones: {})",
                    place.name,
                    place.config.display(),
                    place.clones
                ),
            }
        }
    }
    Ok(())
}

/// `-` for a path `config path`'s text form has nothing to show — a checkout
/// no project claims, where [`ConfigPaths::unclaimed`] leaves every project
/// path `None`.
fn display_opt(path: Option<&Path>) -> String {
    path.map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Both job stores [`config_path`] prints: the user-scoped one in this
/// machine's project home, and the one tracked in the checkout — see
/// [`Repo::user_jobs_file`] and [`Repo::jobs_file`].
#[derive(Debug, PartialEq, Serialize)]
struct JobPaths {
    user: PathBuf,
    project: PathBuf,
}

/// Everything [`config_path`] and [`config_path_anywhere`] print — a plain
/// struct so a test can build one and assert its fields directly, the same
/// way [`render_config_contract`] lets a test assert a string without
/// capturing stdout.
///
/// Every field but `mode` and `workspaces` is `None`/empty only in
/// [`ConfigPaths::unclaimed`]: an ordinary project, repo mode or home mode,
/// fills in all of them, so the three keys `config path` printed before this
/// task keep their old values for a project that already has one — only an
/// unclaimed checkout sees the new `null`s.
#[derive(Debug, PartialEq, Serialize)]
struct ConfigPaths {
    mode: Option<&'static str>,
    setup: Option<PathBuf>,
    /// `None` in home mode or when unclaimed: home mode's whole setup is
    /// already private, so there is no second private layer to report — see
    /// [`crate::local::is_repo_mode`].
    local: Option<PathBuf>,
    overrides: Option<PathBuf>,
    routines: Option<PathBuf>,
    jobs: Option<JobPaths>,
    /// This checkout's own workspace name, home mode only.
    workspace: Option<String>,
    /// Every workspace on the machine, whatever this checkout's own mode —
    /// the skill reads it to propose `spoolway init --workspace <name>`.
    workspaces: Vec<crate::repo::WorkspacePlace>,
}

impl ConfigPaths {
    fn for_repo(repo: &Repo) -> Self {
        // A tracked `.spoolway/` always wins — the same precedence
        // `crate::local::is_repo_mode` gives it — so a project that has one
        // is unambiguously repo mode without ever consulting the workspace
        // registry, which a stray `project.toml` elsewhere on the machine
        // naming this same root by mistake could otherwise contradict.
        let home_clone = (!crate::config::tracked_setup_dir_in(&repo.checkout).is_dir())
            .then(|| crate::repo::workspace_clone(&repo.checkout))
            .flatten();
        let mode = Some(if home_clone.is_some() { "home" } else { "repo" });
        let workspace = home_clone.and_then(|clone| {
            clone
                .workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        ConfigPaths {
            mode,
            setup: Some(repo.setup_dir()),
            local: crate::local::is_repo_mode(&repo.checkout).then(|| repo.local_dir()),
            overrides: Some(repo.overrides_dir()),
            routines: Some(repo.routines_dir()),
            jobs: Some(JobPaths {
                user: repo.user_jobs_file(),
                project: repo.jobs_file(),
            }),
            workspace,
            workspaces: crate::repo::workspace_places(),
        }
    }

    /// What `config path` answers when `Repo::discover` found nothing
    /// claiming this checkout — every project path `None`, but still the
    /// workspace list, since that is what the skill needs before offering
    /// `spoolway init --workspace <name>`.
    fn unclaimed() -> Self {
        ConfigPaths {
            mode: None,
            setup: None,
            local: None,
            overrides: None,
            routines: None,
            jobs: None,
            workspace: None,
            workspaces: crate::repo::workspace_places(),
        }
    }
}

/// `spoolway config contract`: every setting `config.toml` may carry, its
/// values, its default and one sentence about it — rendered from
/// [`crate::confkv::reference_table`], the same register the file's own
/// header table is written from, so a setting is documented once rather than
/// copied a second time into this command's own prose.
pub fn config_contract(repo: &Repo, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    print!("{}", render_config_contract());
    Ok(())
}

/// [`config_contract`]'s body, built as a string so a test can assert it
/// against [`crate::confkv::reference_table`] directly rather than capturing
/// stdout.
fn render_config_contract() -> String {
    let commands: &[(&str, &str)] = &[
        ("spoolway config get <key>", "read one value"),
        (
            "spoolway config set <key> <value>",
            "write one, validated the same way loading the file does",
        ),
        ("spoolway config list", "every settable key, one per line"),
        ("spoolway config show", "the whole file"),
        (
            "spoolway config path",
            "every place the setup lives, and every workspace",
        ),
        (
            "spoolway config edit",
            "open it in $EDITOR, re-validated on save",
        ),
        (
            "spoolway config override",
            "patch it without touching the tracked file — see `spoolway override contract`",
        ),
    ];
    let width = commands.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);

    let mut out = String::new();
    out.push_str("THE CONFIG CONTRACT\n");
    out.push_str("====================\n\n");
    out.push_str(&crate::confkv::reference_table());
    for (cmd, note) in commands {
        out.push_str(&format!("{cmd:width$}  {note}\n"));
    }
    out
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
    open_in_editor(&path)?;

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

/// Resolve the user's editor the way every `spoolway *edit`-shaped command
/// does: `$VISUAL`, then `$EDITOR`, then `vi`.
pub(super) fn editor_from_env() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string())
}

/// Open `path` in the resolved editor and wait for it, refusing on a
/// nonzero exit. Shared by [`config_edit`] and `commands::config_override`,
/// so the quoting fix in [`editor_command`] (finding 71) only has to be
/// right once.
pub(super) fn open_in_editor(path: &Path) -> Result<()> {
    let editor = editor_from_env();
    let status = crate::platform::shell_command(&editor_command(&editor, path))
        .status()
        .with_context(|| format!("could not start `{editor}`"))?;
    if !status.success() {
        bail!("`{editor}` exited with {status}");
    }
    Ok(())
}

/// The shell line that opens `path` in `editor`.
///
/// `$VISUAL`/`$EDITOR` is interpolated unquoted on purpose, so `code --wait`
/// still splits into a program and a flag. The path is quoted through the
/// platform's own escaper, which escapes an embedded `'` — a plain
/// `format!("… '{}'")` would break the quoting on a checkout path that
/// contains one, and the editor would exit on a syntax error (finding 71).
fn editor_command(editor: &str, path: &Path) -> String {
    format!(
        "{editor} {}",
        crate::platform::quote(&path.display().to_string())
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::platform::PathExt;

    /// `config contract` renders from [`crate::confkv::reference_table`]
    /// verbatim rather than a second copy of it — the criterion is that the
    /// two texts agree, and rendering one straight into the other is what
    /// makes that hold by construction rather than by two authors staying in
    /// sync.
    #[test]
    fn config_contract_renders_the_same_reference_table_config_toml_writes() {
        let text = render_config_contract();
        assert!(
            text.contains(&crate::confkv::reference_table()),
            "config contract does not carry config.toml's own header table verbatim"
        );
        assert!(text.contains("dispatch.backend"));
        assert!(text.contains("spoolway config set"));
        assert!(text.contains("spoolway config override"));
    }

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
    fn worktree_fixture(name: &str) -> (Repo, PathBuf, crate::scratch::ScratchRoot) {
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
            "[unattended]\nblocked_agent = \"from-root\"\n",
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
            "[unattended]\nblocked_agent = \"from-worktree\"\n",
        )
        .unwrap();

        // Discovery binds a project to its home on its own now, under a
        // scratch home of its own — never under the real `~/.spoolway/`.
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let home = home.canonical().unwrap();
        let repo =
            crate::platform::test_home::with_home(&home, || crate::repo::Repo::discover(&wt))
                .unwrap();
        assert_ne!(
            repo.checkout, repo.root,
            "the fixture is only useful if it actually lands in a linked worktree"
        );
        (repo, root, base)
    }

    #[test]
    fn config_get_answers_for_the_checkout_not_the_root() {
        let (repo, _root, _base_guard) = worktree_fixture("get");
        config_get(&repo, "unattended.blocked_agent", false).unwrap();
        // `config_get` prints to stdout, which a unit test cannot capture
        // cheaply — so this also asserts the lower-level read it goes
        // through, which is the part that actually decides the answer.
        let config = Config::load(&repo.checkout).unwrap();
        assert_eq!(config.unattended.blocked_agent, "from-worktree");
    }

    #[test]
    fn config_path_names_the_worktrees_own_setup_folder() {
        let (repo, _root, _base_guard) = worktree_fixture("path");
        assert_eq!(repo.setup_dir(), repo.checkout.join(".spoolway"));
    }

    /// `spoolway-config`'s "never reach past" rule names the setup folder,
    /// `local/` and `overrides/` — this is the command it reads all three
    /// from, so the payload it serialises has to actually carry them, and
    /// `local` has to be present in the ordinary repo-mode case.
    #[test]
    fn config_path_reports_setup_local_and_overrides_in_repo_mode() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("config-path-json");
        // `config_path` itself only prints — exercised here too, so a panic
        // in its own printing path still fails this test — but the
        // assertions below are against [`ConfigPaths`], the payload it
        // serialises, since that is what a caller of `--json` actually reads.
        config_path(&repo, false).unwrap();
        config_path(&repo, true).unwrap();

        assert!(crate::local::is_repo_mode(&repo.checkout));
        let paths = ConfigPaths::for_repo(&repo);
        assert_eq!(paths.mode, Some("repo"));
        assert_eq!(paths.setup, Some(repo.checkout.join(".spoolway")));
        assert_eq!(paths.overrides, Some(repo.home.join("overrides")));
        assert_eq!(paths.local, Some(repo.home.join("local")));
        assert_eq!(paths.routines, Some(repo.routines_dir()));
        assert_eq!(
            paths.jobs.as_ref().map(|j| &j.user),
            Some(&repo.user_jobs_file())
        );
        assert_eq!(
            paths.jobs.as_ref().map(|j| &j.project),
            Some(&repo.jobs_file())
        );
        assert_eq!(paths.workspace, None);

        let json = serde_json::to_value(&paths).unwrap();
        assert_eq!(json["mode"], "repo");
        assert_eq!(json["setup"], repo.setup_dir().display().to_string());
        assert_eq!(
            json["overrides"],
            repo.overrides_dir().display().to_string()
        );
        assert_eq!(json["local"], repo.local_dir().display().to_string());
        assert_eq!(json["routines"], repo.routines_dir().display().to_string());
        assert_eq!(
            json["jobs"]["user"],
            repo.user_jobs_file().display().to_string()
        );
        assert_eq!(
            json["jobs"]["project"],
            repo.jobs_file().display().to_string()
        );
        assert!(json["workspace"].is_null());
        assert!(json["workspaces"].is_array());
    }

    /// Home mode's whole setup is already private, so there is no second
    /// private layer for `config path` to report — `local` has to come back
    /// `null`, not the ordinary directory a repo-mode reader would expect.
    #[test]
    fn config_path_reports_no_local_folder_in_home_mode() {
        let root = crate::scratch::root("config-path-home-mode");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        let fake_home = root.parent().unwrap().join(format!(
            "{}-realhome",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&fake_home);

        crate::platform::test_home::with_home(&fake_home, || {
            let clone = crate::repo::create_workspace(&root).unwrap();
            std::fs::create_dir_all(clone.config_dir()).unwrap();
            let home = crate::mux::project_home(&root).unwrap();
            let repo = Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config: Config::default(),
                home,
            };

            assert!(!crate::local::is_repo_mode(&repo.checkout));
            config_path(&repo, true).unwrap();
            let paths = ConfigPaths::for_repo(&repo);
            assert_eq!(paths.mode, Some("home"));
            assert_eq!(paths.setup, Some(clone.config_dir()));
            assert_eq!(paths.local, None);
            assert_eq!(
                paths.workspace.as_deref(),
                clone.workspace.file_name().and_then(|n| n.to_str())
            );

            let json = serde_json::to_value(&paths).unwrap();
            assert_eq!(json["mode"], "home");
            assert!(json["local"].is_null());
            assert_eq!(json["workspace"], paths.workspace.clone().unwrap());
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fake_home).ok();
    }

    #[test]
    fn config_set_refuses_inside_a_linked_worktree_and_writes_nothing() {
        let (repo, root, _base_guard) = worktree_fixture("set");
        let before_root = std::fs::read_to_string(Config::path_in(&root)).unwrap();
        let before_wt = std::fs::read_to_string(Config::path_in(&repo.checkout)).unwrap();

        let err = config_set(&repo, "unattended.blocked_agent", "changed")
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
                "spoolway -C {} config set unattended.blocked_agent changed",
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
        let (repo, _root_guard) = crate::commands::testutil::fixture("config-set-main");
        config_set(&repo, "unattended.blocked_agent", "changed").unwrap();
        let config = Config::load(&repo.root).unwrap();
        assert_eq!(config.unattended.blocked_agent, "changed");
    }

    /// `config list` runs both ways, prints `key = value` rows, and names
    /// every scalar key that resolves to a value today —
    /// `issue_tracking.key_in_names` (the criterion the feature has to meet),
    /// the flat keys the file omits while they hold their default, a profile's
    /// omitted `concurrency`, and a `[models]` glob's omitted fields — all of
    /// it in key order.
    #[test]
    fn config_list_prints_every_settable_key_as_key_equals_value() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("config-list");
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
                .any(|l| l.starts_with("issue_tracking.key_in_names ") && l.ends_with(" = true")),
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

    /// Helper for a bare checkout — git-initialised, no `.spoolway/` ever
    /// written into it — the shape of the `config-path-places` acceptance
    /// criterion "a checkout no project claims".
    fn bare_checkout(name: &str) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        crate::repo::run(
            &root,
            "git",
            &["commit", "-q", "--allow-empty", "-m", "seed"],
        )
        .unwrap();
        root
    }

    /// `Repo::discover` refuses a checkout nothing claims — the one refusal
    /// `config_path_anywhere` treats as an answer rather than a hard
    /// failure, so this is the case the acceptance criterion means by "a
    /// checkout no project claims".
    #[test]
    fn config_path_anywhere_exits_ok_for_a_checkout_no_project_claims() {
        let root = bare_checkout("config-path-anywhere-unclaimed");
        let fake_home = root.parent().unwrap().join(format!(
            "{}-realhome",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&fake_home);

        crate::platform::test_home::with_home(&fake_home, || {
            let err = Repo::discover(&root).unwrap_err();
            assert!(
                Repo::is_unclaimed(&err),
                "the fixture is only useful if the checkout is genuinely unclaimed: {err:#}"
            );

            // A workspace elsewhere on the machine, unrelated to this
            // checkout — still part of the answer, since the skill needs
            // the whole list before proposing `spoolway init --workspace
            // <name>`.
            let other = bare_checkout("config-path-anywhere-other");
            crate::repo::create_workspace(&other).unwrap();

            config_path_anywhere(&root, false).unwrap();
            config_path_anywhere(&root, true).unwrap();

            let places = crate::repo::workspace_places();
            assert_eq!(places.len(), 1, "{places:?}");
            assert!(places[0].error.is_none());
            assert_eq!(places[0].clones, 1);
        });

        std::fs::remove_dir_all(&fake_home).ok();
    }

    /// Review finding on round 1: an unclaimed checkout sitting beside a
    /// workspace whose `project.toml` cannot even be parsed used to make
    /// `Repo::root` bail with "this checkout is in no workspace spoolway can
    /// read, and … does not parse" — a different message from the "no
    /// spoolway project found" [`Repo::is_unclaimed`] originally matched, so
    /// `config_path_anywhere` propagated it as a hard failure instead of
    /// answering `mode: null` with the broken workspace listed. This
    /// reproduces exactly that shape: nothing claims `root`, and the only
    /// thing standing between it and the ordinary "nothing found" refusal is
    /// one broken `project.toml` elsewhere on the machine.
    #[test]
    fn config_path_anywhere_exits_ok_beside_a_broken_workspace_elsewhere() {
        let root = bare_checkout("config-path-anywhere-unclaimed-broken-elsewhere");
        let fake_home = root.parent().unwrap().join(format!(
            "{}-realhome",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&fake_home);

        crate::platform::test_home::with_home(&fake_home, || {
            let state = crate::mux::state_root();
            let broken = state.join("broken-ws");
            std::fs::create_dir_all(broken.join("config")).unwrap();
            std::fs::write(broken.join(crate::repo::BINDING_FILE), "clones = [\n").unwrap();

            // The exact refusal this used to be, and the one
            // `Repo::is_unclaimed` now has to recognise too.
            let err = Repo::discover(&root).unwrap_err();
            assert!(
                format!("{err:#}").contains("does not parse"),
                "the fixture is only useful if the broken file is what `Repo::root` trips on: \
                 {err:#}"
            );
            assert!(
                Repo::is_unclaimed(&err),
                "a broken workspace file elsewhere must not turn an unclaimed checkout into a \
                 hard failure: {err:#}"
            );

            config_path_anywhere(&root, false).unwrap();
            config_path_anywhere(&root, true).unwrap();

            let places = crate::repo::workspace_places();
            assert_eq!(places.len(), 1, "{places:?}");
            assert_eq!(places[0].name, "broken-ws");
            assert!(places[0].error.is_some());
        });

        std::fs::remove_dir_all(&fake_home).ok();
    }

    /// Everything [`ConfigPaths::unclaimed`] is for: `mode: null`, no
    /// project path, but still the workspace list.
    #[test]
    fn config_paths_unclaimed_has_null_mode_and_no_project_paths() {
        let paths = ConfigPaths::unclaimed();
        assert_eq!(paths.mode, None);
        assert_eq!(paths.setup, None);
        assert_eq!(paths.local, None);
        assert_eq!(paths.overrides, None);
        assert_eq!(paths.routines, None);
        assert!(paths.jobs.is_none());
        assert_eq!(paths.workspace, None);

        let json = serde_json::to_value(&paths).unwrap();
        assert!(json["mode"].is_null());
        assert!(json["setup"].is_null());
        assert!(json["overrides"].is_null());
        assert!(json["routines"].is_null());
        assert!(json["jobs"].is_null());
        assert!(json["workspace"].is_null());
        assert!(json["workspaces"].is_array());
    }

    /// Acceptance criterion: an unreadable `project.toml` gives its own
    /// `workspaces` entry an `error` field, and the rest of the list still
    /// prints rather than the whole command failing.
    #[test]
    fn workspace_places_reports_an_error_for_an_unreadable_project_toml() {
        let fake_home = crate::scratch::root("config-path-workspace-places-broken");
        let _ = std::fs::remove_dir_all(&fake_home);
        std::fs::create_dir_all(&fake_home).unwrap();

        crate::platform::test_home::with_home(&fake_home, || {
            let good = bare_checkout("config-path-workspace-places-good");
            crate::repo::create_workspace(&good).unwrap();

            let state = crate::mux::state_root();
            let broken = state.join("broken-ws");
            std::fs::create_dir_all(broken.join("config")).unwrap();
            // Not valid TOML at all, so it fails to parse as a
            // `WorkspaceToml` — the unreadable case this entry's `error`
            // field exists for.
            std::fs::write(broken.join(crate::repo::BINDING_FILE), "clones = [\n").unwrap();

            let places = crate::repo::workspace_places();
            assert_eq!(places.len(), 2, "{places:?}");
            // `create_workspace` names the folder after `good`'s own
            // basename plus a fresh id, never `good`'s path verbatim — so
            // the readable one is told apart from `broken-ws` by having no
            // `error`, not by its exact name.
            assert!(
                places
                    .iter()
                    .any(|p| p.name != "broken-ws" && p.error.is_none()),
                "the readable workspace is still listed: {places:?}"
            );
            let broken_entry = places
                .iter()
                .find(|p| p.name == "broken-ws")
                .unwrap_or_else(|| panic!("the broken workspace is still listed: {places:?}"));
            assert!(broken_entry.error.is_some());
        });

        std::fs::remove_dir_all(&fake_home).ok();
    }
}
