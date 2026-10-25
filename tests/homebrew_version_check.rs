//! The daily version check on a Homebrew install.
//!
//! The built binary is copied to `<tmp>/Cellar/spoolway/0.0.0/bin/spoolway`,
//! which is the layout `Channel::Homebrew` is keyed off. A fake `brew` and a
//! fake `npm` come first on `PATH` and record every call, so the test can show
//! the hidden `__version-check` child asked brew, never ran `brew update`, and
//! never started npm. A second run goes through a `bin/spoolway` symlink into
//! the Cellar copy, the way brew links it.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fake(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A scratch install: fake `brew` and `npm` on `PATH`, and the built binary
/// copied into a Cellar folder.
struct Install {
    root: PathBuf,
    cellar_exe: PathBuf,
}

impl Install {
    fn new(name: &str) -> Install {
        let root = std::env::temp_dir().join(format!("spoolway-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        let cellar_bin = root.join("Cellar/spoolway/0.0.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&cellar_bin).unwrap();
        // The auto-update variable is recorded beside the arguments: brew runs
        // `brew update` itself before some commands unless it is set to 1.
        fake(
            &bin.join("brew"),
            &format!(
                "echo \"$HOMEBREW_NO_AUTO_UPDATE $@\" >> '{}'\n\
                 echo '{{\"formulae\":[{{\"versions\":{{\"stable\":\"99.1.2\"}}}}]}}'\n",
                root.join("brew-calls").display()
            ),
        );
        fake(
            &bin.join("npm"),
            &format!(
                "echo \"$@\" >> '{}'\nexit 1\n",
                root.join("npm-calls").display()
            ),
        );
        let cellar_exe = cellar_bin.join("spoolway");
        std::fs::copy(env!("CARGO_BIN_EXE_spoolway"), &cellar_exe).unwrap();
        Install { root, cellar_exe }
    }

    /// Run `__version-check` through `exe`, from a clean cache and call log.
    fn check(&self, exe: &Path) {
        let _ = std::fs::remove_dir_all(self.root.join("state"));
        let _ = std::fs::remove_file(self.root.join("brew-calls"));
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new(exe)
            .arg("__version-check")
            .current_dir(&self.root)
            .env("PATH", path)
            // Removed so the assertion on the variable means the binary set
            // it, not that a CI image or a dotfile already exported it.
            .env_remove("HOMEBREW_NO_AUTO_UPDATE")
            .env("HOME", &self.root)
            .env("XDG_STATE_HOME", self.root.join("state"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `latest.json` holds the fake brew's version, brew was asked once with
    /// auto-update off and never to update, and npm was never started.
    fn assert_asked_brew_only(&self) {
        let cache = std::fs::read_to_string(self.root.join("state/spoolway/latest.json")).unwrap();
        assert!(cache.contains("99.1.2"), "latest.json: {cache}");
        let brew = std::fs::read_to_string(self.root.join("brew-calls")).unwrap();
        assert_eq!(
            brew.trim(),
            "1 info --json=v2 marvingygas/tap/spoolway",
            "brew calls: {brew}"
        );
        let npm_calls = self.root.join("npm-calls");
        assert!(
            !npm_calls.exists(),
            "npm was called: {}",
            std::fs::read_to_string(&npm_calls).unwrap_or_default()
        );
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `latest.json` ends up holding the fake brew's version, and only brew was asked.
#[test]
fn the_version_check_on_a_homebrew_binary_asks_brew_and_never_npm() {
    let install = Install::new("homebrew-check");
    install.check(&install.cellar_exe);
    install.assert_asked_brew_only();
}

/// Brew links `<prefix>/bin/spoolway` to the Cellar copy, and macOS reports
/// that link as the running executable. The binary still counts as Homebrew.
#[test]
fn the_version_check_through_a_bin_symlink_into_the_cellar_asks_brew() {
    let install = Install::new("homebrew-symlink");
    let link = install.root.join("bin/spoolway");
    std::os::unix::fs::symlink(&install.cellar_exe, &link).unwrap();
    install.check(&link);
    install.assert_asked_brew_only();
}
