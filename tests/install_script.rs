//! `scripts/install.sh`, run with `sh` against a local fake release.
//!
//! The release is a directory of `v<version>/` folders that `curl` reads
//! through a `file://` base URL. A fake `uname` first on `PATH` picks the
//! platform. The script is stamped here the way the release job stamps it, so
//! the test also covers the placeholder the job replaces.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Rig {
    root: PathBuf,
    install_dir: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let root = std::env::temp_dir().join(format!(
            "spoolway-install-script-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let install_dir = root.join("install");
        let script = include_str!("../scripts/install.sh");
        assert!(script.contains("@SPOOLWAY_VERSION@"));
        std::fs::write(
            root.join("install.sh"),
            script.replace("@SPOOLWAY_VERSION@", "0.16.0"),
        )
        .unwrap();
        Rig { root, install_dir }
    }

    fn fake_uname(&self, os: &str, arch: &str) {
        let body =
            format!("#!/bin/sh\ncase \"$1\" in -s) echo '{os}' ;; -m) echo '{arch}' ;; esac\n");
        let path = self.root.join("bin/uname");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
                let out = Command::new("sha256sum").arg(&file).output().unwrap();
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

    fn run(&self, path_dirs: &[&Path]) -> Output {
        let mut path = format!("{}", self.root.join("bin").display());
        for d in path_dirs {
            path.push_str(&format!(":{}", d.display()));
        }
        path.push_str(&format!(":{}", std::env::var("PATH").unwrap_or_default()));
        Command::new("sh")
            .arg(self.root.join("install.sh"))
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("SPOOLWAY_INSTALL_DIR", &self.install_dir)
            .env(
                "SPOOLWAY_BASE_URL",
                format!("file://{}", self.root.join("release").display()),
            )
            .output()
            .unwrap()
    }

    fn installed(&self) -> PathBuf {
        self.install_dir.join("spoolway")
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
             spoolway: {dir} is not on your PATH. Add this line to your\n\
             shell profile, then open a new terminal:\n\
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
             Run spoolway sync in each project to apply the update.\n",
            rig.install_dir.display()
        )
    );
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
