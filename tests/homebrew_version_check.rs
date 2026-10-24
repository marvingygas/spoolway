//! The daily version check on a Homebrew install.
//!
//! The built binary is copied to `<tmp>/Cellar/spoolway/0.0.0/bin/spoolway`,
//! which is the layout `Channel::Homebrew` is keyed off. A fake `brew` and a
//! fake `npm` come first on `PATH` and record every call, so the test can show
//! the hidden `__version-check` child asked brew, never ran `brew update`, and
//! never started npm.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn fake(path: &std::path::Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `latest.json` ends up holding the fake brew's version, and only brew was asked.
#[test]
fn the_version_check_on_a_homebrew_binary_asks_brew_and_never_npm() {
    let root = std::env::temp_dir().join(format!("spoolway-homebrew-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    let cellar_bin = root.join("Cellar/spoolway/0.0.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&cellar_bin).unwrap();
    let brew_calls = root.join("brew-calls");
    let npm_calls = root.join("npm-calls");
    fake(
        &bin.join("brew"),
        &format!(
            "echo \"$@\" >> '{}'\n\
             echo '{{\"formulae\":[{{\"versions\":{{\"stable\":\"99.1.2\"}}}}]}}'\n",
            brew_calls.display()
        ),
    );
    fake(
        &bin.join("npm"),
        &format!("echo \"$@\" >> '{}'\nexit 1\n", npm_calls.display()),
    );
    let exe = cellar_bin.join("spoolway");
    std::fs::copy(env!("CARGO_BIN_EXE_spoolway"), &exe).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let state = root.join("state");
    let output = Command::new(&exe)
        .arg("__version-check")
        .current_dir(&root)
        .env("PATH", path)
        .env("HOME", &root)
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cache = std::fs::read_to_string(state.join("spoolway/latest.json")).unwrap();
    assert!(cache.contains("99.1.2"), "latest.json: {cache}");
    let brew = std::fs::read_to_string(&brew_calls).unwrap();
    assert_eq!(brew.trim(), "info --json=v2 marvingygas/tap/spoolway");
    assert!(!brew.contains("update"), "brew calls: {brew}");
    assert!(
        !npm_calls.exists(),
        "npm was called: {}",
        std::fs::read_to_string(&npm_calls).unwrap_or_default()
    );
    let _ = std::fs::remove_dir_all(&root);
}
