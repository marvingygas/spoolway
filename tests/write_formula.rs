//! `scripts/write-formula.mjs`, run with `node` against a fixture
//! `SHA256SUMS` and `npm/targets.json`.
//!
//! The release job pushes whatever this script prints to the tap, so the
//! test pins the whole formula: every platform must get its own file name and
//! its own checksum, and a missing checksum must stop the script.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn node_available() -> bool {
    Command::new("node").arg("--version").output().is_ok()
}

fn sum(n: u8) -> String {
    format!("{:02x}", n).repeat(32)
}

/// Writes a fixture into a fresh folder and runs the script on it.
fn run(name: &str, sums: &str, targets: &str) -> Output {
    let dir = std::env::temp_dir().join(format!("spoolway-formula-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    std::fs::write(dir.join("targets.json"), targets).unwrap();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/write-formula.mjs");
    let out = Command::new("node")
        .arg(script)
        .args(["0.15.0"])
        .arg(dir.join("SHA256SUMS"))
        .arg(dir.join("targets.json"))
        .output()
        .unwrap();
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
    if !node_available() {
        return;
    }
    let out = run("ok", &sums(), TARGETS);
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
        "class Spoolway < Formula\n  desc \"Descriptive agent workflow builder run by a minimal command line state machine\"\n  homepage \"https://github.com/marvingygas/spoolway\"\n  version \"0.15.0\"\n  license \"MIT\"\n\n  on_macos do\n{}{}  end\n\n  on_linux do\n{}{}  end\n\n  def install\n    bin.install Dir[\"spoolway-*\"].first => \"spoolway\"\n  end\n\n  test do\n    assert_match version.to_s, shell_output(\"#{{bin}}/spoolway --version\")\n  end\nend\n",
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
    if !node_available() {
        return;
    }
    let partial: String = sums()
        .lines()
        .filter(|l| !l.contains("macos-x64"))
        .map(|l| format!("{l}\n"))
        .collect();
    let out = run("missing", &partial, TARGETS);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("spoolway-macos-x64"));
}
