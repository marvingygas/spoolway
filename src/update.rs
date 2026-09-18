//! `spoolway update` — take the newer binary, and nothing else.
//!
//! This used to also bring a project's own files forward: `config.toml`,
//! pipeline key references, skills. That half moved to [`crate::sync`], and
//! what is left here is a fact about this machine rather than about any
//! project — which release npm has, whether this install can take it, and
//! whether a dispatcher is running the file this would replace. None of that
//! needs a `.spoolway/` directory anywhere above the current one, so this
//! runs from any directory, project or not — the one command besides
//! `spoolway init` that does.
//!
//! The split is why this and [`crate::sync`] read almost the same, on
//! purpose: installing a binary is done the moment npm says so and the
//! hand-over lands; bringing a checkout's files forward needs a checkout to
//! land them in, and stays exactly where it always was.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// Take the newer release, if there is one, hand over to it, and say what
/// changed.
///
/// `cwd` is the directory this ran in — `cli.repo` if `-C` was given,
/// otherwise the process's own — never resolved into a project: a project is
/// exactly what this command must not require.
pub fn run(cwd: &Path) -> Result<()> {
    use std::io::IsTerminal;

    if let Some(status) = install(cwd)? {
        // The new binary did the hand-over, and its exit code is the one
        // that means anything — this process has nothing left to say.
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }

    let upgraded = std::env::var(crate::release::ENV_UPGRADED).ok();
    if let Some(previous) = digest_previous(upgraded.as_deref(), std::io::stdout().is_terminal())
        && let Some(digest) = crate::release_notes::update_digest(previous, true)?
    {
        println!();
        print!("{digest}");
    }
    Ok(())
}

/// Whether this invocation is the successful far side of a person-facing npm
/// handover. The terminal gate keeps stdout stable for scripts and pipes; a
/// forged or stale environment value cannot otherwise make a piped run print
/// a digest meant for a person at a terminal.
fn digest_previous(upgraded: Option<&str>, terminal: bool) -> Option<&str> {
    terminal.then_some(upgraded).flatten()
}

/// Take the newer release, if there is one, and hand over to it.
///
/// `Some(status)` means this process is done: the new binary was run in its
/// place and its exit status is the one that matters. `None` means carry on
/// here — nothing newer, nothing installable, or a dispatcher that must not
/// have its binary rewritten under it.
fn install(cwd: &Path) -> Result<Option<std::process::ExitStatus>> {
    let upgrade = crate::release::upgrade(&dispatcher_lock_file(cwd));
    install_upgrade(cwd, upgrade, crate::release::hand_over)
}

/// The project dispatcher's own lock, if `cwd` sits inside a project — a
/// path nothing will ever hold when it does not, so [`crate::release::
/// upgrade`] reads "no project here" the same way it reads "no dispatcher
/// running": there is nothing for a binary swap to interrupt either way.
fn dispatcher_lock_file(cwd: &Path) -> PathBuf {
    match crate::repo::Repo::discover_lenient(cwd) {
        Ok((repo, _, _)) => repo.lock_file(),
        Err(_) => cwd.join(".spoolway-update-no-project.lock"),
    }
}

/// Turn the release layer's exhaustive outcome into update behaviour. Keeping
/// the handover call injectable makes the success edge testable without
/// replacing the running binary or invoking npm; the production caller passes
/// [`crate::release::hand_over`] unchanged.
fn install_upgrade<F>(
    cwd: &Path,
    upgrade: crate::release::Upgrade,
    hand_over: F,
) -> Result<Option<std::process::ExitStatus>>
where
    F: FnOnce(&[String], &str) -> Result<std::process::ExitStatus>,
{
    use crate::release::Upgrade;

    match upgrade {
        Upgrade::Current => Ok(None),

        Upgrade::Installed(version) => {
            println!("Installing spoolway {version}");
            println!(
                "  npm install -g {}@{version} --ignore-scripts",
                crate::release::PACKAGE
            );
            println!();
            Ok(Some(hand_over(&relaunch(cwd), &version)?))
        }

        // The whole reason a channel is resolved before anything is said: an
        // install npm cannot reach is told how it is upgraded instead of being
        // handed a command that would refuse.
        Upgrade::Unmanaged(version) => {
            println!(
                "spoolway {version} is out. This binary was not installed by npm, so upgrade it"
            );
            println!("the way you installed it.");
            println!();
            Ok(None)
        }

        Upgrade::Dispatching(version, pid) => {
            println!("spoolway {version} is out, and a dispatcher is running (pid {pid}), so the");
            println!("binary was left alone — replacing it would kill the run mid-pass.");
            println!();
            Ok(None)
        }
    }
}

/// This command, as the new binary should run it.
///
/// `-C` is passed whether or not it was typed, and that is the point: the
/// child inherits a working directory, not a discovery. A `spoolway -C
/// /elsewhere update` that handed over without it would upgrade the binary
/// while sitting in whichever directory the terminal happened to have out,
/// rather than the one `-C` named.
fn relaunch(cwd: &Path) -> Vec<String> {
    vec![
        "-C".to_string(),
        cwd.display().to_string(),
        "update".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_terminal_interactive_handover_gets_a_digest() {
        assert_eq!(digest_previous(Some("0.1.0"), true), Some("0.1.0"));
        assert_eq!(digest_previous(None, true), None, "no handover happened");
        assert_eq!(digest_previous(Some("0.1.0"), false), None, "piped output");
    }

    fn successful_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    #[test]
    fn every_upgrade_outcome_has_one_explicit_update_path() {
        use crate::release::Upgrade;

        let cwd = crate::scratch::root("update-upgrade-outcomes");
        assert!(
            install_upgrade(&cwd, Upgrade::Current, |_, _| panic!("no handover"))
                .unwrap()
                .is_none()
        );
        assert!(
            install_upgrade(&cwd, Upgrade::Unmanaged("0.2.0".into()), |_, _| panic!(
                "no handover"
            ))
            .unwrap()
            .is_none()
        );
        assert!(
            install_upgrade(
                &cwd,
                Upgrade::Dispatching("0.2.0".into(), 42),
                |_, _| panic!("no handover")
            )
            .unwrap()
            .is_none()
        );

        let status = install_upgrade(&cwd, Upgrade::Installed("0.2.0".into()), |args, version| {
            assert_eq!(args, relaunch(&cwd));
            assert_eq!(version, "0.2.0");
            Ok(successful_status())
        })
        .unwrap()
        .expect("an installed release hands over");
        assert!(status.success());
    }

    /// `-C <dir> update` has to still target that directory after the
    /// handover, whatever directory this process itself is running in.
    #[test]
    fn relaunch_targets_cwd_not_the_process_directory() {
        let cwd = PathBuf::from("/some/elsewhere");
        assert_eq!(
            relaunch(&cwd),
            vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "update".to_string(),
            ]
        );
    }

    /// No project at all: the dispatcher lock file this resolves to must
    /// never exist, so `crate::lock::Lock::holder` reads it as "nothing is
    /// running" rather than erroring — the whole point of `update` working
    /// from a directory with no `.spoolway/` above it.
    #[test]
    fn dispatcher_lock_file_with_no_project_never_exists() {
        let cwd = crate::scratch::root("update-no-project-lock");
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();

        let lock_file = dispatcher_lock_file(&cwd);
        assert!(!lock_file.exists());
        assert!(crate::lock::Lock::holder(&lock_file).unwrap().is_none());
    }
}
