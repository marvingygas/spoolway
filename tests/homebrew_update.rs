//! `spoolway update` on a Homebrew install, driven through the real binary.
//!
//! The built binary is copied to `<tmp>/Cellar/spoolway/0.0.0/bin/spoolway`,
//! the layout `Channel::Homebrew` is keyed off, and a fake `brew` comes first
//! on `PATH` and records every call. Its `upgrade` drops a fake `spoolway` into
//! the same folder, which reports the new version and records the arguments it
//! was handed, standing in for the binary brew would have installed.
//!
//! Four runs: an upgrade that hands over, a refusal while a dispatcher holds
//! the project's lock, a `brew upgrade` that fails, and one that succeeds while
//! another install's `spoolway` is first on `PATH`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// The version the fake brew reports, newer than any this crate ships.
const NEW_VERSION: &str = "99.1.2";

const CHECKING: &str = "Checking Homebrew for a newer spoolway...\n";

const INSTALLING: &str = "Installing spoolway 99.1.2\n  brew upgrade marvingygas/tap/spoolway\n\n";

fn fake(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What the fake `brew upgrade` does.
#[derive(Clone, Copy)]
enum Upgrade {
    /// Installs a `spoolway` that reports the new version.
    Works,
    /// Prints an error and exits 1.
    Fails,
    /// Installs a `spoolway` that still reports an old version, as a stale
    /// copy earlier on `PATH` would.
    StaysOld,
}

struct Install {
    root: PathBuf,
    project: PathBuf,
    cellar_exe: PathBuf,
}

impl Install {
    /// A project, a Cellar copy of the binary, and a fake `brew` whose
    /// `upgrade` does what `upgrade` says.
    fn new(name: &str, upgrade: Upgrade) -> Install {
        let root = std::env::temp_dir().join(format!("spoolway-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        let cellar_bin = root.join("Cellar/spoolway/0.0.0/bin");
        let project = root.join("project");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&cellar_bin).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        let git = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&project)
            .status()
            .unwrap();
        assert!(git.success(), "git init failed");

        let reported = match upgrade {
            Upgrade::StaysOld => "0.0.1",
            _ => NEW_VERSION,
        };
        let upgrade = match upgrade {
            Upgrade::Fails => {
                "echo 'Error: the formula could not be built' >&2\nexit 1\n".to_string()
            }
            Upgrade::Works | Upgrade::StaysOld => format!(
                "echo '==> Upgrading marvingygas/tap/spoolway'\n\
                 printf '#!/bin/sh\\n\
                 case \"$1\" in --version) echo spoolway {reported};; \
                 *) echo \"$@\" > \"{handed}\"; echo handed over;; esac\\n' > '{bin}/spoolway'\n\
                 chmod +x '{bin}/spoolway'\n",
                handed = root.join("handed-over").display(),
                bin = bin.display()
            ),
        };
        fake(
            &bin.join("brew"),
            &format!(
                "echo \"$@\" >> '{calls}'\n\
                 case \"$1\" in\n\
                 update) exit 0;;\n\
                 info) echo '{{\"formulae\":[{{\"versions\":{{\"stable\":\"{NEW_VERSION}\"}}}}]}}';;\n\
                 upgrade)\n{upgrade};;\n\
                 esac\n",
                calls = root.join("brew-calls").display()
            ),
        );
        let cellar_exe = cellar_bin.join("spoolway");
        std::fs::copy(env!("CARGO_BIN_EXE_spoolway"), &cellar_exe).unwrap();
        Install {
            root,
            project,
            cellar_exe,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(&self.cellar_exe)
            .args(args)
            .current_dir(&self.project)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("SPOOLWAY_SKIP_VERSION_CHECK", "1")
            .env_remove("SPOOLWAY_UPGRADED")
            .output()
            .unwrap()
    }

    fn brew_calls(&self) -> String {
        std::fs::read_to_string(self.root.join("brew-calls")).unwrap_or_default()
    }

    /// Make the project's dispatcher lock name `pid`, the way a running
    /// dispatcher would. The start-time line is left empty, which the lock
    /// reads as "judge by the pid alone".
    fn hold_dispatch_lock(&self, pid: u32) {
        let init = self.run(&["init", "--yes", "--provider", "claude", "--tracker", "none"]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let home = std::fs::read_dir(self.root.join("home/.spoolway"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|dir| dir.join("project.toml").exists())
            .expect("the project's home");
        std::fs::write(home.join("dispatch.pid"), format!("{pid}\n\n")).unwrap();
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const BREW_INFO: &str = "info --json=v2 marvingygas/tap/spoolway";

/// A newer release and no dispatcher: brew updates, answers, upgrades with its
/// output on the terminal, and the new binary is handed `-C <project> update`.
#[test]
fn update_on_a_homebrew_binary_upgrades_through_brew_and_hands_over() {
    let install = Install::new("homebrew-update", Upgrade::Works);
    let output = install.run(&["update"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{CHECKING}{INSTALLING}==> Upgrading marvingygas/tap/spoolway\n\nhanded over\n")
    );
    assert_eq!(
        install.brew_calls(),
        format!("update\n{BREW_INFO}\nupgrade marvingygas/tap/spoolway\n")
    );
    let handed = std::fs::read_to_string(install.root.join("handed-over")).unwrap();
    assert!(handed.starts_with("-C "), "handed: {handed}");
    assert!(handed.trim_end().ends_with(" update"), "handed: {handed}");
}

/// A dispatcher holds the lock: the refusal is printed whole, and brew is
/// asked for the version but never to upgrade.
#[test]
fn update_on_a_homebrew_binary_refuses_while_a_dispatcher_runs() {
    let install = Install::new("homebrew-refused", Upgrade::Works);
    let mut holder: Child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    install.hold_dispatch_lock(holder.id());
    let output = install.run(&["update"]);
    let _ = holder.kill();
    let _ = holder.wait();

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "{CHECKING}spoolway {NEW_VERSION} is out, and a dispatcher is running (pid {}), so the\n\
             binary was left alone — replacing it would kill the run mid-pass.\n\
             Stop the dispatcher, then run spoolway update again.\n\n",
            holder.id()
        )
    );
    assert_eq!(install.brew_calls(), format!("update\n{BREW_INFO}\n"));
    assert!(!install.root.join("handed-over").exists());
}

/// `brew upgrade` exits non-zero: its own error comes first, then the lines
/// naming the running version, the exit is non-zero and nothing is handed over.
#[test]
fn update_on_a_homebrew_binary_reports_a_failed_brew_upgrade() {
    let install = Install::new("homebrew-failed", Upgrade::Fails);
    let output = install.run(&["update"]);

    assert_eq!(output.status.code(), Some(1), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("Error: the formula could not be built"),
        "stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        stdout(&output),
        format!(
            "{CHECKING}{INSTALLING}\
             \nbrew upgrade did not finish, so spoolway {} is still installed.\n\
             Run it yourself to see the whole error:\n\n  \
             brew upgrade marvingygas/tap/spoolway\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(
        install.brew_calls(),
        format!("update\n{BREW_INFO}\nupgrade marvingygas/tap/spoolway\n")
    );
    assert!(!install.root.join("handed-over").exists());
}

/// `brew upgrade` succeeds but the `spoolway` first on `PATH` still reports the
/// old version: nothing is handed over, the exit is non-zero, and the refusal
/// names the folder brew links into, so the person knows what to put first.
#[test]
fn update_on_a_homebrew_binary_refuses_a_stale_spoolway_first_on_path() {
    let install = Install::new("homebrew-stale", Upgrade::StaysOld);
    let output = install.run(&["update"]);

    assert_eq!(output.status.code(), Some(1), "stdout: {}", stdout(&output));
    let stderr = stderr(&output);
    let bin = install.root.join("bin");
    assert!(
        stderr.contains(&format!(
            "the `spoolway` on PATH resolves to `{}`",
            bin.join("spoolway").display()
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&format!("put `{}` first on PATH", bin.display())),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("then run spoolway update again"),
        "stderr: {stderr}"
    );
    assert!(!install.root.join("handed-over").exists());
}
