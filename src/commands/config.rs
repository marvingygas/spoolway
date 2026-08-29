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

    let status = crate::platform::shell_command(&format!("{editor} '{}'", path.display()))
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

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
}
