//! `spoolway update` on a binary npm did not install.
//!
//! The built binary lives under `target/`, which is not a global npm install,
//! so it is standalone. A fake `npm` first on `PATH` records every call, and
//! the test checks it was never called while the install line was printed.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// A standalone binary prints the install line, exits 0, and starts no npm.
#[test]
fn update_on_a_standalone_binary_prints_the_install_line_and_runs_no_npm() {
    let root =
        std::env::temp_dir().join(format!("spoolway-standalone-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = root.join("npm-calls");
    let fake = bin.join("npm");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
        .arg("update")
        .current_dir(&root)
        .env("PATH", path)
        .env("HOME", &root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env_remove("SPOOLWAY_UPGRADED")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = format!(
        "This spoolway ({}) was not installed by npm, so it does not\n\
         update itself. To get the latest release, run the install line again:\n\
         \n\
         \x20 curl -fsSL https://github.com/marvingygas/spoolway/releases/latest/download/install.sh | sh\n",
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout, expected);
    assert!(
        !calls.exists(),
        "npm was called: {}",
        std::fs::read_to_string(&calls).unwrap_or_default()
    );
    let _ = std::fs::remove_dir_all(&root);
}
