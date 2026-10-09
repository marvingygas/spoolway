//! A `config.toml` holding keys this binary does not know, driven through the
//! real binary: it loads and names them, `doctor` lists them, and `config set`
//! and `sync` keep them with the comments above them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spoolway-unknown-keys-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create project");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .output()
            .expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// [`Project::run`] without the success assertion, for `doctor`: a fresh
    /// project has unrelated problems to report (no model set on its agent
    /// steps), and exits non-zero for them.
    fn run_allowing_failure(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .output()
            .expect("run spoolway")
    }

    fn config_path(&self) -> PathBuf {
        self.0.join(".spoolway/config.toml")
    }
}

impl AsRef<Path> for Project {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("output is utf-8")
}

#[test]
fn unknown_keys_load_are_named_and_survive_set_and_sync() {
    let project = Project::new();
    project.run(&["init", "--yes", "--provider", "claude", "--tracker", "none"]);

    let mut config = std::fs::read_to_string(project.config_path()).expect("read config");
    config.push_str(
        "\n# a setting from the future\n[dispatch.future]\nunused = 1\n\
         \n# I meant [unattended]\n[unatended]\nmax_cost = 3\n",
    );
    // A typo inside a known table, with a comment of the person's own.
    config = config.replacen(
        "[dispatch]\n",
        "[dispatch]\n# spelt wrong on purpose\nlane_quite = \"5m\"\n",
        1,
    );
    std::fs::write(project.config_path(), &config).expect("write config");

    // Loads, and says what it passed over.
    let doctor = project.run_allowing_failure(&["doctor"]);
    let said = format!("{}{}", text(&doctor.stdout), text(&doctor.stderr));
    for name in ["dispatch.lane_quite", "unatended"] {
        assert!(said.contains(name), "doctor did not name {name}: {said}");
    }
    assert!(said.contains("note: "), "{said}");
    assert!(
        said.contains("does not know: "),
        "doctor has no row for the unknown keys: {said}"
    );

    // `config set` edits in place and keeps every one of them.
    project.run(&["config", "set", "dispatch.keep_finished_lanes", "false"]);
    let after_set = std::fs::read_to_string(project.config_path()).expect("read config");
    assert!(
        after_set.contains("# spelt wrong on purpose\nlane_quite = \"5m\""),
        "{after_set}"
    );
    assert!(
        after_set.contains("# I meant [unattended]\n[unatended]"),
        "{after_set}"
    );

    // `sync` rewrites the whole file, and still keeps them.
    let sync = project.run(&["sync"]);
    let after_sync = std::fs::read_to_string(project.config_path()).expect("read config");
    assert!(
        after_sync.contains("# spelt wrong on purpose\nlane_quite = \"5m\""),
        "{after_sync}\n{}",
        text(&sync.stdout)
    );
    assert!(
        after_sync.contains("# I meant [unattended]\n[unatended]"),
        "{after_sync}"
    );
    assert!(
        after_sync.contains("# a setting from the future\n[dispatch.future]\nunused = 1"),
        "{after_sync}"
    );
}
