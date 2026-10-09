//! A running dispatcher re-reads `config.toml` on every pass, driven through the
//! real binary on the headless test backend with a stand-in agent that never
//! reports.
//!
//! Three tasks in separate groups compete for one `claude` slot. Raising
//! `agents.claude.concurrency` to 2 mid-run must start the second without a
//! restart. A config that stops parsing must leave the run on the last good
//! one, which the third task's wait line shows by still reading `(2/2)`, and
//! must say so once.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Two pass intervals (the dispatcher waits ten seconds between idle passes)
/// plus slack for a slow machine.
const PASSES: Duration = Duration::from_secs(45);

struct Project {
    root: PathBuf,
    dispatcher: Option<Child>,
}

impl Project {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let base = std::env::temp_dir().join(format!(
            "spoolway-live-reload-{}-{nonce}",
            std::process::id()
        ));
        let root = base.join("proj");
        fs::create_dir_all(&root).expect("create project");
        fs::create_dir_all(base.join("home")).expect("create home");
        fs::create_dir_all(base.join("bin")).expect("create bin");
        // Holds its lane open until `stop` appears, so the slot stays taken
        // for as long as the test needs it and nothing is left running after.
        let stand_in = base.join("bin/claude");
        fs::write(
            &stand_in,
            "#!/bin/sh\necho \"$SPOOLWAY_TASK\" >> \"$STAND_IN_LOG\"\n\
             while [ ! -e \"$STAND_IN_STOP\" ]; do sleep 1; done\n",
        )
        .expect("write stand-in agent");
        make_executable(&stand_in);
        let project = Self {
            root,
            dispatcher: None,
        };
        project.git(&["init", "-q", "-b", "main"]);
        project.git(&["config", "user.email", "spoolway@example.invalid"]);
        project.git(&["config", "user.name", "spoolway tests"]);
        fs::write(project.root.join("seed"), "seed").expect("write seed");
        project.git(&["add", "-A"]);
        project.git(&["commit", "-qm", "seed"]);
        project.git(&["checkout", "-q", "-b", "plan/demo"]);
        project
    }

    fn base(&self) -> &Path {
        self.root.parent().expect("project has a parent")
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn command(&self) -> Command {
        let path = format!(
            "{}:{}",
            self.base().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_spoolway"));
        command
            .current_dir(&self.root)
            .env("HOME", self.base().join("home"))
            .env("PATH", path)
            .env("SPOOLWAY_TEST_BACKEND", "1")
            .env("STAND_IN_LOG", self.log())
            .env("STAND_IN_STOP", self.base().join("stop"));
        // A test run from inside a lane inherits that lane's identity, and
        // the binary then refuses to mutate the queue.
        for (key, _) in std::env::vars() {
            if key.starts_with("SPOOLWAY_") && key != "SPOOLWAY_TEST_BACKEND"
                || key.starts_with("HERDR_")
            {
                command.env_remove(key);
            }
        }
        command
    }

    fn run(&self, args: &[&str]) {
        let output = self.command().args(args).output().expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn log(&self) -> PathBuf {
        self.base().join("stand-in.log")
    }

    fn dispatch_out(&self) -> PathBuf {
        self.base().join("dispatch.out")
    }

    fn started(&self) -> Vec<String> {
        fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn dispatch_text(&self) -> String {
        fs::read_to_string(self.dispatch_out()).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut(&Project) -> bool) {
        let deadline = Instant::now() + PASSES;
        while Instant::now() < deadline {
            if done(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        panic!(
            "timed out waiting for {what}\nstarted: {:?}\ndispatcher said:\n{}",
            self.started(),
            self.dispatch_text()
        );
    }

    fn task(&self, id: &str, group: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: job\ngroup: {group}\npipeline: solo\nbase: plan/demo\n---\n\n\
             ## Intend\n\nx\n"
        )
    }

    fn set_up(&self) {
        self.run(&["init", "--yes"]);
        self.run(&["config", "set", "dispatch.backend", "headless"]);
        self.run(&["config", "set", "agents.claude.concurrency", "1"]);
        let pipelines = self.root.join(".spoolway/pipelines");
        for entry in fs::read_dir(&pipelines).expect("read pipelines") {
            fs::remove_file(entry.expect("entry").path()).expect("remove shipped pipeline");
        }
        fs::write(
            pipelines.join("solo.yml"),
            "steps:\n  - id: work\n    agent: claude\n    model: claude-sonnet-5-5\n    on_pass: done\n",
        )
        .expect("write pipeline");
        let prompt = self.root.join(".spoolway/prompts/work");
        fs::create_dir_all(&prompt).expect("create prompt dir");
        fs::write(prompt.join("PROMPT.md"), "do it\n").expect("write prompt");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", "configure"]);
        for (id, group) in [("ta", "ga"), ("tb", "gb"), ("tc", "gc")] {
            let file = self.base().join(format!("{id}.md"));
            fs::write(&file, self.task(id, group)).expect("write task");
            self.run(&[
                "queue",
                "add",
                "--from",
                file.to_str().expect("utf-8 path"),
                "--base",
                "plan/demo",
            ]);
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::write(self.base().join("stop"), "");
        if let Some(mut dispatcher) = self.dispatcher.take() {
            let _ = dispatcher.kill();
            let _ = dispatcher.wait();
        }
        // Lets the stand-ins notice `stop` before their directory goes.
        std::thread::sleep(Duration::from_millis(1500));
        let _ = fs::remove_dir_all(self.base());
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[cfg(not(unix))]
fn make_executable(_: &Path) {}

#[cfg(unix)]
#[test]
fn raising_concurrency_mid_run_starts_the_second_lane_and_a_bad_config_is_reported_once() {
    let mut project = Project::new();
    project.set_up();

    let out = fs::File::create(project.dispatch_out()).expect("create dispatcher log");
    let child = project
        .command()
        .arg("dispatch")
        .stdout(out.try_clone().expect("clone log"))
        .stderr(out)
        .stdin(Stdio::null())
        .spawn()
        .expect("start dispatcher");
    project.dispatcher = Some(child);

    // One slot: the first task takes it and the second is told why it waits.
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    project.wait_for("the second task to be held for a slot", |p| {
        p.dispatch_text()
            .contains("waiting for a `claude` slot (1/1)")
    });
    assert_eq!(project.started(), ["ta"], "only one lane fits the cap");

    // The same command a person types; no restart follows it.
    project.run(&["config", "set", "agents.claude.concurrency", "2"]);
    project.wait_for("the second lane to start on the raised cap", |p| {
        p.started().len() == 2
    });
    assert_eq!(project.started(), ["ta", "tb"]);
    project.wait_for("the third task to be held on the raised cap", |p| {
        p.dispatch_text()
            .contains("waiting for a `claude` slot (2/2)")
    });

    // A config that stops parsing: the run keeps going on the last good one
    // and names the failure on the pass that found it, not on every pass.
    let config = project.root.join(".spoolway/config.toml");
    let good = fs::read_to_string(&config).expect("read config");
    fs::write(&config, "this is = = not toml\n").expect("break config");
    let notice = "config did not reload, still running on the last good one";
    project.wait_for("the reload failure to be reported", |p| {
        p.dispatch_text().contains(notice)
    });
    let passes_before = project.dispatch_text().matches("next pass in").count();
    project.wait_for("two more passes over the broken config", |p| {
        p.dispatch_text().matches("next pass in").count() >= passes_before + 2
    });
    // Falling back to the defaults would change the cap, and with it the
    // figures on this line.
    let text = project.dispatch_text();
    let after_break = &text[text.find(notice).expect("notice was printed")..];
    assert!(
        after_break.contains("waiting for a `claude` slot (2/2)"),
        "the run left the last good config:\n{text}"
    );
    assert_eq!(
        project.dispatch_text().matches(notice).count(),
        1,
        "the same error must not be printed every pass:\n{}",
        project.dispatch_text()
    );
    fs::write(&config, good).expect("restore config");
}
