//! `scripts/write-formula.mjs` and `scripts/tap-state.mjs`, run with `node`.
//!
//! The release job pushes whatever the first script prints to the tap, so the
//! tests pin the whole formula: every platform must get its own file name and
//! its own checksum, and anything that could pair a platform with the wrong
//! file must stop the script. The second script decides whether the job may
//! push at all, so its cases cover every ordering of the tap's version against
//! the release's.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// A machine without node would otherwise pass every test here by running
/// nothing, so a missing node stops the run with a sentence that names it.
fn require_node() {
    assert!(
        Command::new("node").arg("--version").output().is_ok(),
        "host has no node, so scripts/write-formula.mjs and scripts/tap-state.mjs cannot be tested. Install node and run the tests again."
    );
}

fn sum(n: u8) -> String {
    format!("{:02x}", n).repeat(32)
}

fn script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

/// Writes a fixture into a fresh folder and runs the script on it. With no
/// `targets` the script gets no third argument, so it reads the repository's
/// own `npm/targets.json` as the release job does.
fn run(name: &str, version: &str, sums: &str, targets: Option<&str>) -> Output {
    require_node();
    let dir = std::env::temp_dir().join(format!("spoolway-formula-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    let mut cmd = Command::new("node");
    cmd.arg(script("write-formula.mjs"))
        .arg(version)
        .arg(dir.join("SHA256SUMS"));
    if let Some(targets) = targets {
        std::fs::write(dir.join("targets.json"), targets).unwrap();
        cmd.arg(dir.join("targets.json"));
    }
    let out = cmd.output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

const TARGETS: &str = r#"{"targets":[
 {"os":"linux","cpu":"x64","libc":"glibc","download":null},
 {"os":"linux","cpu":"arm64","libc":"glibc","download":null},
 {"os":"linux","cpu":"x64","libc":"musl","download":"spoolway-linux-x64"},
 {"os":"linux","cpu":"arm64","libc":"musl","download":"spoolway-linux-arm64"},
 {"os":"darwin","cpu":"arm64","libc":null,"download":"spoolway-macos-arm64"},
 {"os":"darwin","cpu":"x64","libc":null,"download":"spoolway-macos-x64"}]}"#;

fn sums() -> String {
    format!(
        "{}  ./install.sh\n{}  ./spoolway-linux-arm64\n{}  ./spoolway-linux-x64\n{}  ./spoolway-macos-arm64\n{}  ./spoolway-macos-x64\n",
        sum(1),
        sum(2),
        sum(3),
        sum(4),
        sum(5)
    )
}

/// Every platform gets its own url and checksum, and nothing else changes.
#[test]
fn formula_pairs_each_platform_with_its_own_file_and_checksum() {
    let out = run("ok", "0.15.0", &sums(), Some(TARGETS));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let base = "https://github.com/marvingygas/spoolway/releases/download/v0.15.0";
    let block = |key: &str, file: &str, n: u8| {
        format!(
            "    {key} do\n      url \"{base}/{file}\"\n      sha256 \"{}\"\n    end\n",
            sum(n)
        )
    };
    let want = format!(
        "class Spoolway < Formula\n  desc \"Descriptive agent workflow builder run by a minimal command-line state machine\"\n  homepage \"https://github.com/marvingygas/spoolway\"\n  license \"MIT\"\n\n  on_macos do\n{}{}  end\n\n  on_linux do\n{}{}  end\n\n  def install\n    bin.install Dir[\"spoolway-*\"].first => \"spoolway\"\n  end\n\n  test do\n    assert_match version.to_s, shell_output(\"#{{bin}}/spoolway --version\")\n  end\nend\n",
        block("on_arm", "spoolway-macos-arm64", 4),
        block("on_intel", "spoolway-macos-x64", 5),
        block("on_arm", "spoolway-linux-arm64", 2),
        block("on_intel", "spoolway-linux-x64", 3),
    );
    assert_eq!(String::from_utf8(out.stdout).unwrap(), want);
}

/// A download without a checksum line stops the script and names the file.
#[test]
fn missing_checksum_is_an_error() {
    let partial: String = sums()
        .lines()
        .filter(|l| !l.contains("macos-x64"))
        .map(|l| format!("{l}\n"))
        .collect();
    let out = run("missing", "0.15.0", &partial, Some(TARGETS));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("spoolway-macos-x64"));
}

/// A prerelease gets a `version` line between `homepage` and `license`,
/// because Homebrew would read "64" from the "arm64" in its urls. A release
/// gets none, because `brew audit --strict` rejects one that repeats the url.
#[test]
fn only_a_prerelease_gets_a_version_line() {
    let rc = run("rc", "0.15.0-rc.1", &sums(), Some(TARGETS));
    assert!(rc.status.success());
    let rc = String::from_utf8(rc.stdout).unwrap();
    assert!(
        rc.contains("  homepage \"https://github.com/marvingygas/spoolway\"\n  version \"0.15.0-rc.1\"\n  license \"MIT\"\n"),
        "{rc}"
    );
    assert!(rc.contains("/releases/download/v0.15.0-rc.1/spoolway-macos-arm64"));

    let plain = run("plain", "0.15.0", &sums(), Some(TARGETS));
    assert!(
        !String::from_utf8(plain.stdout)
            .unwrap()
            .contains("version \"")
    );
}

/// A target list that lost a platform's download stops the script and names
/// the platform, instead of printing a formula that cannot install there.
#[test]
fn a_platform_without_a_download_is_an_error() {
    let targets = TARGETS.replace(r#""download":"spoolway-macos-x64""#, r#""download":null"#);
    let out = run("nodownload", "0.15.0", &sums(), Some(&targets));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no download file for darwin x64"));
}

/// Two targets with one os and cpu would leave the pairing to file order, so
/// the script refuses and names both targets.
#[test]
fn two_downloads_for_one_platform_are_an_error() {
    let targets = TARGETS.replace(
        r#"{"os":"linux","cpu":"x64","libc":"glibc","download":null}"#,
        r#"{"pkg":"linux-x64","os":"linux","cpu":"x64","libc":"glibc","download":"spoolway-linux-x64-glibc"}"#,
    );
    let targets = targets.replace(
        r#"{"os":"linux","cpu":"x64","libc":"musl","#,
        r#"{"pkg":"linux-x64-musl","os":"linux","cpu":"x64","libc":"musl","#,
    );
    let out = run("dup", "0.15.0", &sums(), Some(&targets));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("two downloads for linux/x64"), "{err}");
    assert!(
        err.contains("spoolway-linux-x64-glibc (linux-x64)"),
        "{err}"
    );
    assert!(err.contains("spoolway-linux-x64 (linux-x64-musl)"), "{err}");
}

/// With no targets argument the script reads the repository's own
/// `npm/targets.json`, as the release job does, so a renamed os or cpu value
/// there fails here and not on a tag.
#[test]
fn the_real_targets_file_gives_four_platform_blocks() {
    let out = run("real", "0.15.0", &sums(), None);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let formula = String::from_utf8(out.stdout).unwrap();
    for (file, n) in [
        ("spoolway-macos-arm64", 4),
        ("spoolway-macos-x64", 5),
        ("spoolway-linux-arm64", 2),
        ("spoolway-linux-x64", 3),
    ] {
        let pair = format!(
            "url \"https://github.com/marvingygas/spoolway/releases/download/v0.15.0/{file}\"\n      sha256 \"{}\"",
            sum(n)
        );
        assert!(formula.contains(&pair), "missing {file}:\n{formula}");
    }
}

/// Runs `tap-state.mjs` against a formula that names `tap` (or no formula).
fn tap_state(tap: Option<&str>, version: &str) -> Output {
    require_node();
    let dir = std::env::temp_dir().join(format!(
        "spoolway-tap-state-{}-{}",
        tap.unwrap_or("none"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if let Some(tap) = tap {
        std::fs::write(
            dir.join("spoolway.rb"),
            format!(
                "class Spoolway < Formula\n    on_arm do\n      url \"https://github.com/marvingygas/spoolway/releases/download/v{tap}/spoolway-macos-arm64\"\n    end\nend\n"
            ),
        )
        .unwrap();
    }
    let out = Command::new("node")
        .arg(script("tap-state.mjs"))
        .arg(version)
        .arg(dir.join("spoolway.rb"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// The push is skipped for the same version and for a newer one, so an old
/// tag's re-run cannot replace a newer formula. Prereleases order as semver
/// orders them, and `rc.2` comes before `rc.10`.
#[test]
fn tap_state_orders_versions_like_semver() {
    let cases = [
        (None, "0.15.0", "behind"),
        (Some("0.14.0"), "0.15.0", "behind"),
        (Some("0.15.0"), "0.15.0", "same"),
        (Some("0.16.0"), "0.15.0", "newer"),
        (Some("0.15.0"), "0.15.0-rc.1", "newer"),
        (Some("0.15.0-rc.1"), "0.15.0", "behind"),
        (Some("0.15.0-rc.2"), "0.15.0-rc.10", "behind"),
        (Some("0.15.0-rc.1"), "0.15.0-rc.1", "same"),
        (Some("0.9.0"), "0.10.0", "behind"),
    ];
    for (tap, version, want) in cases {
        let out = tap_state(tap, version);
        assert!(
            out.status.success(),
            "{tap:?} vs {version}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            want,
            "tap {tap:?} against release {version}"
        );
    }
}

/// A formula in the tap with no release url cannot be ordered, so the script
/// stops with exit 2 and a sentence instead of guessing.
#[test]
fn tap_state_refuses_a_formula_without_a_release_url() {
    require_node();
    let dir = std::env::temp_dir().join(format!("spoolway-tap-state-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("spoolway.rb"), "class Spoolway < Formula\nend\n").unwrap();
    let out = Command::new("node")
        .arg(script("tap-state.mjs"))
        .arg("0.15.0")
        .arg(dir.join("spoolway.rb"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be read"));
}
