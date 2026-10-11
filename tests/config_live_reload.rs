//! A running dispatcher re-reads `config.toml` on every pass, driven through the
//! real binary on the headless test backend with a stand-in agent that only
//! reports when asked to.
//!
//! Four tasks in separate groups compete for one `claude` slot. Raising
//! `agents.claude.concurrency` to 2 mid-run must start the second without a
//! restart. A config that stops parsing must leave the run on the last good
//! one, which the third task's wait line shows by still reading `(2/2)`, and
//! must say so once. A lane that reports while the file is broken must still
//! land its report, and so must the dispatcher over a `config.toml` that has
//! been moved aside.

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
             while [ ! -e \"$STAND_IN_STOP\" ]; do\n\
             if [ -e \"$STAND_IN_STOP.report.$SPOOLWAY_TASK\" ]; then\n\
             \"$SPOOLWAY_UNDER_TEST\" report --pass -m done \
             2> \"$STAND_IN_STOP.stderr.$SPOOLWAY_TASK\"\n\
             echo $? > \"$STAND_IN_STOP.status.$SPOOLWAY_TASK\"\n\
             break\nfi\nsleep 1\ndone\n",
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
            .env("STAND_IN_STOP", self.base().join("stop"))
            .env("SPOOLWAY_UNDER_TEST", env!("CARGO_BIN_EXE_spoolway"));
        // A test run from inside a lane inherits that lane's identity, and
        // the binary then refuses to mutate the queue.
        for (key, _) in std::env::vars() {
            if key.starts_with("SPOOLWAY_")
                && key != "SPOOLWAY_TEST_BACKEND"
                && key != "SPOOLWAY_UNDER_TEST"
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

    /// Has the stand-in for `task` report while the config is in whatever
    /// state the caller left it, and answer its exit status and stderr.
    fn report_from_lane(&self, task: &str) -> (String, String) {
        let stop = self.base().join("stop");
        let marker = |kind: &str| PathBuf::from(format!("{}.{kind}.{task}", stop.display()));
        fs::write(marker("report"), "").expect("ask the lane to report");
        self.wait_for("the lane's report to finish", |_| marker("status").exists());
        (
            fs::read_to_string(marker("status")).unwrap_or_default(),
            fs::read_to_string(marker("stderr")).unwrap_or_default(),
        )
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
        for (id, group) in [("ta", "ga"), ("tb", "gb"), ("tc", "gc"), ("td", "gd")] {
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

    // A lane reporting over the broken file runs on the last good config
    // instead of dying before it records anything.
    // Its slot goes to the next waiting task, which keeps the cap full for
    // the fourth to be held against.
    let (status, stderr) = project.report_from_lane("ta");
    assert_eq!(
        status.trim(),
        "0",
        "report failed over a broken config:\n{stderr}"
    );
    assert!(
        stderr.contains("last good config"),
        "report did not say which config it ran on:\n{stderr}"
    );

    // `spoolway stack` reads the config the same way. The task it is handed
    // does not exist, so it fails after the config is settled — the note on
    // stderr is the proof it got that far on the last good one.
    let stack = project
        .command()
        .args(["stack", "no-such-task"])
        .output()
        .expect("run spoolway stack");
    let stack_said = String::from_utf8_lossy(&stack.stderr).into_owned();
    assert!(
        stack_said.contains("running on the last good config"),
        "stack did not fall back over a broken config:\n{stack_said}"
    );

    // The file gone altogether is the same kind of failure, not the defaults.
    fs::remove_file(&config).expect("move config aside");
    let missing = "does not exist";
    project.wait_for("the missing file to be reported", |p| {
        p.dispatch_text().contains(missing)
    });
    let passes_before = project.dispatch_text().matches("next pass in").count();
    project.wait_for("two more passes without the file", |p| {
        p.dispatch_text().matches("next pass in").count() >= passes_before + 2
    });
    let text = project.dispatch_text();
    let after_missing = &text[text.rfind(missing).expect("problem was printed")..];
    assert!(
        after_missing.contains("td: waiting for a `claude` slot (2/2)"),
        "a missing config sent the run back to defaults:\n{text}"
    );
    assert_eq!(
        project.started(),
        ["ta", "tb", "tc"],
        "no lane beyond the cap"
    );

    // A lane reporting while the file is gone runs on the last good config
    // too, and names the file it could not find.
    let (status, stderr) = project.report_from_lane("tb");
    assert_eq!(
        status.trim(),
        "0",
        "report failed over a missing config:\n{stderr}"
    );
    assert!(
        stderr.contains("last good config") && stderr.contains("does not exist"),
        "report did not say the file was missing:\n{stderr}"
    );
    fs::write(&config, good).expect("restore config");
}
