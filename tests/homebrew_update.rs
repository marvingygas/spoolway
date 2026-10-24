//! `spoolway update` on a Homebrew install, driven through the real binary.
//!
//! The built binary is copied to `<tmp>/Cellar/spoolway/0.0.0/bin/spoolway`,
//! the layout `Channel::Homebrew` is keyed off, and a fake `brew` comes first
//! on `PATH` and records every call. Its `upgrade` drops a fake `spoolway` into
//! the same folder, which reports the new version and records the arguments it
//! was handed, standing in for the binary brew would have installed.
//!
//! Five runs: an upgrade that hands over, a refusal while a dispatcher holds
//! the project's lock, a `brew upgrade` that fails, one that succeeds while
//! another install's `spoolway` is first on `PATH`, and a Ctrl-C while
//! `brew update` is still fetching.

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

        let upgrade = match upgrade {
            Upgrade::Fails => {
                "echo 'Error: the formula could not be built' >&2\nexit 1\n".to_string()
            }
            Upgrade::Works => format!(
                "echo '==> Upgrading marvingygas/tap/spoolway'\n\
                 printf '#!/bin/sh\\n\
                 case \"$1\" in --version) echo spoolway {NEW_VERSION};; \
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
                 update)\n\
                 if [ -f '{hangs}' ]; then sleep 30 & echo $! > '{fetch}.tmp'; \
                 mv '{fetch}.tmp' '{fetch}'; wait; fi\n\
                 exit 0;;\n\
                 info) echo '{{\"formulae\":[{{\"versions\":{{\"stable\":\"{NEW_VERSION}\"}}}}]}}';;\n\
                 upgrade)\n{upgrade};;\n\
                 esac\n",
                calls = root.join("brew-calls").display(),
                hangs = root.join("update-hangs").display(),
                fetch = root.join("update-fetch.pid").display()
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
        self.command(&[], args).output().unwrap()
    }

    /// The Cellar binary with `args`, with `first` and then `<root>/bin` ahead
    /// of the rest of `PATH`.
    fn command(&self, first: &[PathBuf], args: &[&str]) -> Command {
        let mut path: Vec<PathBuf> = first.to_vec();
        path.push(self.root.join("bin"));
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(&self.cellar_exe);
        command
            .args(args)
            .current_dir(&self.project)
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("SPOOLWAY_SKIP_VERSION_CHECK", "1")
            .env_remove("SPOOLWAY_UPGRADED");
        command
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

/// `brew upgrade` succeeds, but another install's `spoolway`, earlier on
/// `PATH` than brew's folder, still reports an old version. Nothing is handed
/// over, the exit is non-zero, and the refusal names the stale copy and tells
/// the person to put brew's folder first, which is not the stale copy's.
#[test]
fn update_on_a_homebrew_binary_refuses_a_stale_spoolway_first_on_path() {
    let install = Install::new("homebrew-stale", Upgrade::Works);
    let stale = install.root.join("stale");
    std::fs::create_dir_all(&stale).unwrap();
    fake(
        &stale.join("spoolway"),
        &format!(
            "case \"$1\" in --version) echo spoolway 0.0.1;; \
             *) echo \"$@\" > '{}';; esac\n",
            install.root.join("stale-handed-over").display()
        ),
    );
    let output = install
        .command(std::slice::from_ref(&stale), &["update"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "stdout: {}", stdout(&output));
    let stderr = stderr(&output);
    assert!(
        stderr.contains(&format!(
            "the `spoolway` on PATH resolves to `{}`",
            stale.join("spoolway").display()
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "put `{}` first on PATH",
            install.root.join("bin").display()
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("then run spoolway update again"),
        "stderr: {stderr}"
    );
    assert!(!install.root.join("handed-over").exists());
    assert!(!install.root.join("stale-handed-over").exists());
}

/// Ctrl-C while `brew update` is still fetching. brew runs in a process group
/// of its own, which the terminal's Ctrl-C does not reach, so `spoolway` has to
/// stop it before it goes; a fetch left running would hold brew's update lock.
/// The signal is sent to `spoolway` alone, which is all a Ctrl-C reaches of
/// this run, and `spoolway` still ends by it.
#[test]
fn ctrl_c_during_the_lookup_stops_brew_update_too() {
    use std::os::unix::process::ExitStatusExt;

    let install = Install::new("homebrew-interrupted", Upgrade::Works);
    std::fs::write(install.root.join("update-hangs"), "").unwrap();
    let mut update = install
        .command(&[], &["update"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let fetch_pid_file = install.root.join("update-fetch.pid");
    assert!(
        within(20, || fetch_pid_file.exists()),
        "brew update never started its fetch"
    );
    let fetch: libc::pid_t = std::fs::read_to_string(&fetch_pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // SAFETY: `kill` has no memory-safety preconditions.
    unsafe { libc::kill(update.id() as libc::pid_t, libc::SIGINT) };
    assert!(
        within(10, || matches!(update.try_wait(), Ok(Some(_)))),
        "spoolway update did not end on Ctrl-C"
    );
    let status = update.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");

    // A process nobody has reaped yet still answers signal 0, so it counts as
    // gone once it is a zombie.
    let gone = || {
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(fetch, 0) } == 0;
        let zombie = std::fs::read_to_string(format!("/proc/{fetch}/stat"))
            .is_ok_and(|stat| stat.contains(") Z"));
        !alive || zombie
    };
    let left = within(3, gone);
    if !left {
        // SAFETY: as above; the test cleans up what it started.
        unsafe { libc::kill(fetch, libc::SIGKILL) };
    }
    assert!(left, "brew update's fetch outlived spoolway's Ctrl-C");
    assert_eq!(install.brew_calls(), "update\n");
}

/// Whether `done` holds within `seconds`, asking every 20 ms.
fn within(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    done()
}
