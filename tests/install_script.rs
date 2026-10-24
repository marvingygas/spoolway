//! `scripts/install.sh`, run with `sh` against a local fake release.
//!
//! The release is a directory of `v<version>/` folders that a fake `curl` or
//! `wget` reads through a `file://` base URL. The script runs with a `PATH`
//! that holds only symlinks to the few tools it needs, a fake `uname` and the
//! fake download and hash tools, so the branch it takes is chosen by the test
//! and not by what the host happens to have installed. The script is stamped
//! here the way the release job stamps it, so the test also covers the
//! placeholder the job replaces.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Tools the script calls that are not shell builtins and not faked.
const BASE_TOOLS: [&str; 9] = [
    "awk", "grep", "head", "mktemp", "cp", "chmod", "mv", "rm", "mkdir",
];

struct Rig {
    root: PathBuf,
    install_dir: PathBuf,
}

/// The first of `names` found on the host's `PATH`.
fn host_tool(names: &[&str]) -> Option<(String, PathBuf)> {
    let path = std::env::var("PATH").unwrap_or_default();
    names.iter().find_map(|name| {
        path.split(':')
            .map(|d| Path::new(d).join(name))
            .find(|p| p.is_file())
            .map(|p| (name.to_string(), p))
    })
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Rig {
    fn new(name: &str) -> Rig {
        let root = std::env::temp_dir().join(format!(
            "spoolway-install-script-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for tool in BASE_TOOLS {
            let (_, real) = host_tool(&[tool]).unwrap_or_else(|| panic!("host has no {tool}"));
            std::os::unix::fs::symlink(real, root.join("bin").join(tool)).unwrap();
        }
        let install_dir = root.join("install");
        let script = include_str!("../scripts/install.sh");
        assert!(script.contains("@SPOOLWAY_VERSION@"));
        std::fs::write(
            root.join("install.sh"),
            script.replace("@SPOOLWAY_VERSION@", "0.16.0"),
        )
        .unwrap();
        let rig = Rig { root, install_dir };
        rig.tools("curl", "sha256sum");
        rig
    }

    /// Replaces the stamped script with the file as it sits in the repository.
    fn unstamp(&self) {
        std::fs::write(
            self.root.join("install.sh"),
            include_str!("../scripts/install.sh"),
        )
        .unwrap();
    }

    /// Puts exactly one downloader (`curl` or `wget`) and one hasher
    /// (`sha256sum` or `shasum`) on the script's `PATH`. Both read `file://`
    /// URLs by copying, and both hash with whichever real tool the host has.
    fn tools(&self, downloader: &str, hasher: &str) {
        let bin = self.root.join("bin");
        for name in ["curl", "wget", "sha256sum", "shasum"] {
            let _ = std::fs::remove_file(bin.join(name));
        }
        let flag = if downloader == "curl" { "-o" } else { "-O" };
        write_exec(
            &bin.join(downloader),
            &format!(
                "#!/bin/sh\nout=\nwhile [ $# -gt 1 ]; do\n  [ \"$1\" = {flag} ] && {{ out=$2; shift; }}\n  shift\ndone\ncp \"${{1#file://}}\" \"$out\"\n"
            ),
        );
        let (real_name, real) = host_tool(&["sha256sum", "shasum"]).expect("a sha-256 tool");
        let real_args = if real_name == "shasum" { "-a 256 " } else { "" };
        let drop_args = if hasher == "shasum" { "shift 2\n" } else { "" };
        write_exec(
            &bin.join(hasher),
            &format!(
                "#!/bin/sh\n{drop_args}exec {} {real_args}\"$@\"\n",
                real.display()
            ),
        );
    }

    fn fake_uname(&self, os: &str, arch: &str) {
        let body =
            format!("#!/bin/sh\ncase \"$1\" in -s) echo '{os}' ;; -m) echo '{arch}' ;; esac\n");
        write_exec(&self.root.join("bin/uname"), &body);
    }

    /// Publishes `v0.16.0/<asset>` whose `--version` says `version`, with a
    /// SHA256SUMS line made the way the release job makes it.
    fn release(&self, asset: &str, version: &str, sums_hash: Option<&str>) {
        let dir = self.root.join("release/v0.16.0");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(asset);
        std::fs::write(&file, format!("#!/bin/sh\necho 'spoolway {version}'\n")).unwrap();
        let hash = match sums_hash {
            Some(h) => h.to_string(),
            None => {
                let (name, real) = host_tool(&["sha256sum", "shasum"]).expect("a sha-256 tool");
                let mut cmd = Command::new(real);
                if name == "shasum" {
                    cmd.args(["-a", "256"]);
                }
                let out = cmd.arg(&file).output().unwrap();
                String::from_utf8(out.stdout)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_string()
            }
        };
        std::fs::write(dir.join("SHA256SUMS"), format!("{hash}  ./{asset}\n")).unwrap();
    }

    /// The script's command with the install directory left to the caller.
    fn command(&self, path_dirs: &[&Path]) -> Command {
        let mut path = self.root.join("bin").display().to_string();
        for d in path_dirs {
            path.push_str(&format!(":{}", d.display()));
        }
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(self.root.join("install.sh"))
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.root)
            .env(
                "SPOOLWAY_BASE_URL",
                format!("file://{}", self.root.join("release").display()),
            );
        cmd
    }

    fn run(&self, path_dirs: &[&Path]) -> Output {
        self.command(path_dirs)
            .env("SPOOLWAY_INSTALL_DIR", &self.install_dir)
            .output()
            .unwrap()
    }

    fn installed(&self) -> PathBuf {
        self.install_dir.join("spoolway")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn text(o: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

/// A first install prints three lines plus the PATH hint, and the file runs.
#[test]
fn a_first_install_writes_the_file_and_hints_the_path() {
    let rig = Rig::new("first");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    let dir = rig.install_dir.display();
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "spoolway: downloading spoolway-linux-x64 (v0.16.0)\n\
             spoolway: checksum verified\n\
             spoolway: installed v0.16.0 -> {dir}/spoolway\n\
             \n\
             spoolway: {dir} is not on your PATH. Add this line to your\n\
             shell profile, then open a new terminal:\n\
             \n\
             \x20 export PATH=\"{dir}:$PATH\"\n"
        )
    );
    let run = Command::new(rig.installed())
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "spoolway 0.16.0\n");
    let leftovers: Vec<_> = std::fs::read_dir(&rig.install_dir).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "a temporary file was left behind");
}

/// A rerun over an older copy names both versions, prints the sync line, and
/// stays quiet about PATH when the directory is on it.
#[test]
fn a_replace_names_both_versions_and_prints_the_sync_line() {
    let rig = Rig::new("replace");
    rig.fake_uname("Darwin", "arm64");
    rig.release("spoolway-macos-arm64", "0.16.0", None);
    std::fs::create_dir_all(&rig.install_dir).unwrap();
    std::fs::write(rig.installed(), "#!/bin/sh\necho 'spoolway 0.15.0'\n").unwrap();
    std::fs::set_permissions(rig.installed(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = rig.run(&[&rig.install_dir]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        stdout,
        format!(
            "spoolway: downloading spoolway-macos-arm64 (v0.16.0)\n\
             spoolway: checksum verified\n\
             spoolway: replaced v0.15.0 with v0.16.0 -> {}/spoolway\n\
             \n\
             Run spoolway sync in each project to apply the update.\n",
            rig.install_dir.display()
        )
    );
}

/// A rerun over a copy of the same version replaces it but prints no sync
/// line, because no project has anything new to apply.
#[test]
fn a_same_version_rerun_prints_no_sync_line() {
    let rig = Rig::new("same");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    std::fs::create_dir_all(&rig.install_dir).unwrap();
    std::fs::write(rig.installed(), "#!/bin/sh\necho 'spoolway 0.16.0'\n").unwrap();
    std::fs::set_permissions(rig.installed(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = rig.run(&[&rig.install_dir]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.contains("replaced v0.16.0 with v0.16.0"), "{stdout}");
    assert!(!stdout.contains("spoolway sync"), "{stdout}");
}

/// A bad checksum refuses with exit 1 and leaves the old file and directory
/// exactly as they were.
#[test]
fn a_checksum_mismatch_refuses_and_changes_nothing() {
    let rig = Rig::new("mismatch");
    rig.fake_uname("Linux", "aarch64");
    rig.release("spoolway-linux-arm64", "0.16.0", Some(&"0".repeat(64)));
    std::fs::create_dir_all(&rig.install_dir).unwrap();
    std::fs::write(rig.installed(), "old").unwrap();
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr,
        "spoolway: checksum mismatch for spoolway-linux-arm64 (v0.16.0).\n\
         The download is damaged or was changed. Nothing was installed.\n"
    );
    assert!(!stdout.contains("installed"), "{stdout}");
    assert_eq!(std::fs::read_to_string(rig.installed()).unwrap(), "old");
    assert_eq!(std::fs::read_dir(&rig.install_dir).unwrap().count(), 1);
}

/// A host with no file is refused before any download, and the install
/// directory is never created.
#[test]
fn an_unsupported_platform_refuses_and_writes_nothing() {
    let rig = Rig::new("unsupported");
    rig.fake_uname("MINGW64_NT-10.0-26100", "x86_64");
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "spoolway: no standalone build for MINGW64_NT-10.0-26100/x86_64.\n\
         On Windows, run spoolway inside WSL.\n"
    );
    assert!(!rig.install_dir.exists());
}

/// With no `SPOOLWAY_INSTALL_DIR` the file lands in `$HOME/.local/bin`, and the
/// hint writes that directory as `$HOME/.local/bin` for the profile.
#[test]
fn the_default_directory_is_under_home() {
    let rig = Rig::new("default");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    let out = rig.command(&[]).output().unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(rig.root.join(".local/bin/spoolway").is_file());
    assert!(
        stdout.ends_with("\n  export PATH=\"$HOME/.local/bin:$PATH\"\n"),
        "{stdout}"
    );
}

/// With neither `HOME` nor `SPOOLWAY_INSTALL_DIR` the script says what to set
/// instead of stopping on an unset variable.
#[test]
fn no_home_and_no_install_dir_refuses_with_advice() {
    let rig = Rig::new("nohome");
    rig.fake_uname("Linux", "x86_64");
    let out = rig.command(&[]).env_remove("HOME").output().unwrap();
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr.contains("Set SPOOLWAY_INSTALL_DIR"), "{stderr}");
}

/// A trailing slash on the install directory neither doubles the slash in the
/// printed path nor hides the directory from the PATH check.
#[test]
fn a_trailing_slash_is_dropped_from_the_install_directory() {
    let rig = Rig::new("slash");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    let with_slash = PathBuf::from(format!("{}/", rig.install_dir.display()));
    let out = rig
        .command(&[&rig.install_dir])
        .env("SPOOLWAY_INSTALL_DIR", &with_slash)
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stdout.contains(&format!("-> {}/spoolway\n", rig.install_dir.display())),
        "{stdout}"
    );
    assert!(!stdout.contains("PATH"), "{stdout}");
}

/// An unset `HOME` does not matter when the install directory is given: the
/// default is only built from `HOME` if it is needed.
#[test]
fn no_home_is_fine_when_the_install_directory_is_set() {
    let rig = Rig::new("nohome-dir");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    let out = rig
        .command(&[])
        .env_remove("HOME")
        .env("SPOOLWAY_INSTALL_DIR", &rig.install_dir)
        .output()
        .unwrap();
    let (_, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(rig.installed().is_file());
}

/// A backslash in the install directory prints unchanged. This guards the
/// `printf` calls against a return to `echo`, which expands `\n` on a host
/// whose `/bin/sh` is dash and so only fails there.
#[test]
fn a_backslash_in_the_install_directory_prints_unchanged() {
    let mut rig = Rig::new("backslash");
    rig.install_dir = rig.root.join("a\\nb");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    let dir = rig.install_dir.display();
    assert!(out.status.success(), "{stderr}");
    assert!(
        stdout.contains(&format!("installed v0.16.0 -> {dir}/spoolway\n")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("{dir} is not on your PATH.")),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(&format!("  export PATH=\"{dir}:$PATH\"\n")),
        "{stdout}"
    );
}

/// A directory where the file should go is refused, not filled.
#[test]
fn a_directory_at_the_target_is_refused_and_left_alone() {
    let rig = Rig::new("dirtarget");
    rig.fake_uname("Linux", "x86_64");
    rig.release("spoolway-linux-x64", "0.16.0", None);
    std::fs::create_dir_all(rig.installed()).unwrap();
    let out = rig.run(&[]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr.contains("is a directory"), "{stderr}");
    assert_eq!(std::fs::read_dir(rig.installed()).unwrap().count(), 0);
}

/// The unstamped file in the repository refuses to run and writes nothing.
#[test]
fn an_unstamped_script_refuses() {
    let rig = Rig::new("unstamped");
    rig.unstamp();
    rig.fake_uname("Linux", "x86_64");
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "spoolway: this copy of install.sh has no release version stamped into it.\n\
         Run the install line from the README, which fetches the stamped copy from a release.\n"
    );
    assert!(!rig.install_dir.exists());
}

/// A host that is neither Windows nor a supported platform is told what to do
/// instead, and nothing is written.
#[test]
fn an_unsupported_unix_refuses_with_a_build_from_source_line() {
    let rig = Rig::new("freebsd");
    rig.fake_uname("FreeBSD", "amd64");
    let out = rig.run(&[]);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout, "");
    assert!(
        stderr.starts_with("spoolway: no standalone build for FreeBSD/amd64.\n"),
        "{stderr}"
    );
    assert!(stderr.contains("cargo install --path ."), "{stderr}");
    assert!(!rig.install_dir.exists());
}

/// Every download name in npm/targets.json installs on the host that
/// `uname` would report for it, so the script's file names and the names the
/// release uploads cannot drift apart.
#[test]
fn every_download_named_in_targets_json_installs() {
    let targets: serde_json::Value =
        serde_json::from_str(include_str!("../npm/targets.json")).unwrap();
    let mut seen = 0;
    for t in targets["targets"].as_array().unwrap() {
        let Some(download) = t["download"].as_str() else {
            continue;
        };
        let os = match t["os"].as_str().unwrap() {
            "linux" => "Linux",
            "darwin" => "Darwin",
            other => panic!("targets.json has an os this test does not know: {other}"),
        };
        let arch = match t["cpu"].as_str().unwrap() {
            "x64" => "x86_64",
            "arm64" => "arm64",
            other => panic!("targets.json has a cpu this test does not know: {other}"),
        };
        let rig = Rig::new(&format!("targets-{download}"));
        rig.fake_uname(os, arch);
        rig.release(download, "0.16.0", None);
        let out = rig.run(&[]);
        let (_, stderr) = text(&out);
        assert!(out.status.success(), "{download}: {stderr}");
        assert!(rig.installed().is_file(), "{download} was not installed");
        seen += 1;
    }
    assert_eq!(seen, 4, "targets.json should name four downloads");
}

/// The script installs with either downloader and either hasher, whichever
/// the host has, because the test picks which one is on its `PATH`.
#[test]
fn each_downloader_and_hasher_works() {
    for (downloader, hasher) in [
        ("curl", "sha256sum"),
        ("curl", "shasum"),
        ("wget", "sha256sum"),
        ("wget", "shasum"),
    ] {
        let rig = Rig::new(&format!("tools-{downloader}-{hasher}"));
        rig.tools(downloader, hasher);
        rig.fake_uname("Linux", "x86_64");
        rig.release("spoolway-linux-x64", "0.16.0", None);
        let out = rig.run(&[]);
        let (_, stderr) = text(&out);
        assert!(out.status.success(), "{downloader}/{hasher}: {stderr}");
        assert!(rig.installed().is_file(), "{downloader}/{hasher}");
    }
}
