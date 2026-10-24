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
//!
//! A request to reload, written as a file in the project home, makes the same
//! dispatcher load the pipelines again at the top of its next pass. A good one
//! swaps the graph in and rewrites the snapshot lanes report on. One that
//! would strand a task on a step that is gone, or whose pipeline file does not
//! parse, is refused and leaves the run as it was.
//!
//! `spoolway dispatch reload` writes that request itself and prints the
//! answer: `successfully loaded` and exit 0, or the whole refusal and a
//! non-zero exit. With no dispatcher running it writes nothing.
//!
//! A reload also sets the run's mode to `unattended.enabled`. Switching on
//! starts the spend ceilings' window at the reload, so what was spent before it
//! is not counted. Switching off lifts the ceilings from the next pass.

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
            .env("SPOOLWAY_TEST_BACKEND", "headless")
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

    /// Starts `spoolway dispatch` in the project, logging to `dispatch_out`.
    fn start_dispatcher(&mut self) {
        let out = fs::File::create(self.dispatch_out()).expect("create dispatcher log");
        let child = self
            .command()
            .arg("dispatch")
            .stdout(out.try_clone().expect("clone log"))
            .stderr(out)
            .stdin(Stdio::null())
            .spawn()
            .expect("start dispatcher");
        self.dispatcher = Some(child);
    }

    /// The project's folder under the test's `HOME`, found by the snapshot
    /// the running dispatcher writes into it.
    fn project_home(&self) -> PathBuf {
        let mut found = None;
        self.wait_for("the dispatcher's snapshot", |p| {
            found = fs::read_dir(p.base().join("home/.spoolway"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .find(|dir| dir.join("dispatch-pipelines.json").exists());
            found.is_some()
        });
        found.expect("project home")
    }

    fn snapshot(&self) -> String {
        fs::read_to_string(self.project_home().join("dispatch-pipelines.json"))
            .expect("read snapshot")
    }

    /// Writes a request the way `reload::request` does, and answers what the
    /// dispatcher said to it.
    fn reload(&self, id: &str) -> String {
        let home = self.project_home();
        let requests = home.join("reload/requests");
        fs::create_dir_all(&requests).expect("create requests dir");
        fs::write(requests.join(id), id).expect("write request");
        let answer = home.join(format!("reload/answers/{id}.json"));
        self.wait_for("the reload to be answered", |_| answer.exists());
        let text = fs::read_to_string(answer).expect("read answer");
        assert!(
            text.contains(&format!("\"request\":\"{id}\"")),
            "the answer does not name its request:\n{text}"
        );
        text
    }

    /// Runs `spoolway dispatch reload` and waits for it to exit.
    fn reload_command(&self) -> std::process::Output {
        self.command()
            .args(["dispatch", "reload"])
            .output()
            .expect("run spoolway dispatch reload")
    }

    /// The third line of the dispatcher's lock file: `attended` or
    /// `unattended`.
    fn lock_mode(&self) -> String {
        fs::read_to_string(self.project_home().join("dispatch.pid"))
            .expect("read lock")
            .lines()
            .nth(2)
            .unwrap_or_default()
            .to_string()
    }

    /// Banks `output` tokens against a task, stamped now, the way a finished
    /// lane would.
    fn spend(&self, output: u64) {
        let line = format!(
            "{{\"ts\":\"{}\",\"task\":\"spent\",\"tokens\":{{\"output\":{output}}}}}\n",
            chrono::Utc::now().to_rfc3339()
        );
        let ledger = self.project_home().join("usage.jsonl");
        let mut text = fs::read_to_string(&ledger).unwrap_or_default();
        text.push_str(&line);
        fs::write(ledger, text).expect("write ledger");
    }

    fn pipeline_file(&self) -> PathBuf {
        self.root.join(".spoolway/pipelines/solo.yml")
    }

    fn task_file(&self, id: &str) -> PathBuf {
        self.project_home().join("queue").join(format!("{id}.md"))
    }

    fn set_up(&self) {
        self.run(&["init", "--yes"]);
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

/// Two steps instead of one, the second with a prompt of its own.
const TWO_STEPS: &str = "steps:\n  - id: work\n    agent: claude\n    model: claude-sonnet-5-5\n    \
    on_pass: extra\n  - id: extra\n    agent: claude\n    model: claude-sonnet-5-5\n    \
    on_pass: done\n";

#[cfg(unix)]
#[test]
fn a_reload_request_swaps_in_an_edited_pipeline_before_the_next_lane_reports() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);

    let prompt = project.root.join(".spoolway/prompts/extra");
    fs::create_dir_all(&prompt).expect("create prompt dir");
    fs::write(prompt.join("PROMPT.md"), "do the rest\n").expect("write prompt");
    fs::write(project.pipeline_file(), TWO_STEPS).expect("edit pipeline");
    assert!(
        !project.snapshot().contains("\"extra\""),
        "the run took up an edit nobody asked it to"
    );
    let check = project
        .command()
        .args(["pipeline", "check"])
        .output()
        .expect("run pipeline check");
    assert!(
        String::from_utf8_lossy(&check.stdout).contains("solo.yml changed since"),
        "pipeline check does not name the waiting edit:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );

    let answer = project.reload("1-1");
    assert!(answer.contains("Loaded"), "reload was not taken:\n{answer}");
    assert!(
        project.snapshot().contains("\"extra\""),
        "the snapshot does not hold the new graph"
    );
    let check = project
        .command()
        .args(["pipeline", "check"])
        .output()
        .expect("run pipeline check");
    assert!(
        !String::from_utf8_lossy(&check.stdout).contains("changed since"),
        "pipeline check still names the reloaded file:\n{}",
        String::from_utf8_lossy(&check.stdout)
    );

    // The lane that was already running reports on the new graph, so its
    // task moves to the step that exists only there.
    let (status, stderr) = project.report_from_lane("ta");
    assert_eq!(status.trim(), "0", "report failed:\n{stderr}");
    let task = fs::read_to_string(project.task_file("ta")).expect("read task");
    assert!(
        task.contains("stage: extra"),
        "the report did not route on the new graph:\n{task}"
    );
}

#[cfg(unix)]
#[test]
fn a_reload_is_refused_when_a_task_waits_on_a_step_the_edit_deleted() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    let before = project.snapshot();

    // `tb` has no lane, so it waits on the step it was left on.
    let waiting = project.task_file("tb");
    let text = fs::read_to_string(&waiting).expect("read task");
    fs::write(&waiting, text.replace("stage: queued", "stage: work")).expect("park task");
    fs::write(
        project.pipeline_file(),
        "steps:\n  - id: other\n    agent: claude\n    model: claude-sonnet-5-5\n    \
         on_pass: done\n",
    )
    .expect("edit pipeline");

    let answer = project.reload("2-1");
    assert!(
        answer.contains("Refused"),
        "reload was not refused:\n{answer}"
    );
    assert!(
        answer.contains("tb: sitting on `work`, which pipeline `solo` no longer defines")
            && answer.contains("Put `work` back in solo.yml, or wait until tb has left it"),
        "the refusal does not name the task and the fix:\n{answer}"
    );
    assert_eq!(project.snapshot(), before, "a refusal rewrote the snapshot");
}

#[cfg(unix)]
#[test]
fn a_reload_is_refused_when_a_pipeline_file_does_not_parse() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    let before = project.snapshot();

    fs::write(project.pipeline_file(), "steps: [this is: not a pipeline\n").expect("break file");

    let answer = project.reload("3-1");
    assert!(
        answer.contains("Refused"),
        "reload was not refused:\n{answer}"
    );
    assert!(
        answer.contains("the pipelines do not load"),
        "the refusal does not say why:\n{answer}"
    );
    assert_eq!(project.snapshot(), before, "a refusal rewrote the snapshot");
}

#[cfg(unix)]
#[test]
fn the_reload_command_prints_that_it_loaded_and_the_run_takes_the_edit() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);

    let prompt = project.root.join(".spoolway/prompts/extra");
    fs::create_dir_all(&prompt).expect("create prompt dir");
    fs::write(prompt.join("PROMPT.md"), "do the rest\n").expect("write prompt");
    fs::write(project.pipeline_file(), TWO_STEPS).expect("edit pipeline");

    let output = project.reload_command();
    assert!(
        output.status.success(),
        "the command did not exit 0: {output:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "successfully loaded"
    );
    assert!(
        project.snapshot().contains("\"extra\""),
        "the snapshot does not hold the new graph"
    );
}

#[cfg(unix)]
#[test]
fn the_reload_command_prints_the_whole_refusal_and_exits_non_zero() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    let before = project.snapshot();

    // `ta` is running on `work`, and `tb` has no lane, so it waits on the
    // step it was left on. The refusal names both.
    let waiting = project.task_file("tb");
    let text = fs::read_to_string(&waiting).expect("read task");
    fs::write(&waiting, text.replace("stage: queued", "stage: work")).expect("park task");
    fs::write(
        project.pipeline_file(),
        "steps:\n  - id: other\n    agent: claude\n    model: claude-sonnet-5-5\n    \
         on_pass: done\n",
    )
    .expect("edit pipeline");

    let output = project.reload_command();
    assert!(!output.status.success(), "a refusal exited 0: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "reload refused — nothing changed, the run is still on what it had:\n  \
         ta: sitting on `work`, which pipeline `solo` no longer defines\n  \
         Put `work` back in solo.yml, or wait until ta has left it, then reload again.\n  \
         tb: sitting on `work`, which pipeline `solo` no longer defines\n  \
         Put `work` back in solo.yml, or wait until tb has left it, then reload again."
    );
    assert_eq!(project.snapshot(), before, "a refusal rewrote the snapshot");
}

#[cfg(unix)]
#[test]
fn the_reload_command_with_no_dispatcher_says_so_and_writes_no_request() {
    let project = Project::new();
    project.run(&["init", "--yes"]);

    let output = project.reload_command();
    assert!(!output.status.success(), "it exited 0: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "no dispatcher is running, so there is nothing to reload.\n  \
         spoolway dispatch    # a new run loads every file as it is now"
    );
    let requests = fs::read_dir(project.base().join("home/.spoolway"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|home| home.path().join("reload/requests"))
        .filter(|dir| dir.exists())
        .flat_map(|dir| fs::read_dir(dir).expect("read requests").flatten())
        .count();
    assert_eq!(requests, 0, "a request was left with nobody to read it");
}

#[cfg(unix)]
#[test]
fn the_reload_command_reaches_the_dispatcher_over_a_config_that_does_not_parse() {
    let mut project = Project::new();
    project.set_up();
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    let before = project.snapshot();

    fs::write(project.root.join(".spoolway/config.toml"), "[agents\n").expect("break config");

    let output = project.reload_command();
    assert!(!output.status.success(), "a refusal exited 0: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("reload refused — nothing changed")
            && stderr.contains("config.toml does not load")
            && stderr.contains("Fix config.toml, then reload again."),
        "the dispatcher's own refusal did not reach the terminal:\n{stderr}"
    );
    assert_eq!(project.snapshot(), before, "a refusal rewrote the snapshot");
}

#[cfg(unix)]
#[test]
fn the_reload_command_with_no_dispatcher_and_a_broken_config_still_says_so() {
    let project = Project::new();
    project.run(&["init", "--yes"]);
    fs::write(project.root.join(".spoolway/config.toml"), "[agents\n").expect("break config");

    let output = project.reload_command();
    assert!(!output.status.success(), "it exited 0: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with("no dispatcher is running, so there is nothing to reload."),
        "{output:?}"
    );
}

/// A reload that turns unattended on counts the ceiling from the reload, not
/// from the start of the run. The 5000 tokens banked before it are left out,
/// so the ceiling reports only the 5000 banked after it.
#[cfg(unix)]
#[test]
fn a_reload_that_turns_unattended_on_counts_spend_from_the_reload() {
    let mut project = Project::new();
    project.set_up();
    project.run(&["config", "set", "unattended.max_output_tokens", "1000"]);
    project.run(&[
        "config",
        "set",
        "unattended.blocked_model",
        "claude-sonnet-5-5",
    ]);
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    assert_eq!(project.lock_mode(), "attended");
    project.spend(5000);
    std::thread::sleep(Duration::from_millis(200));

    project.run(&["config", "set", "unattended.enabled", "true"]);
    let answer = project.reload("2-1");
    assert!(answer.contains("Loaded"), "reload was not taken:\n{answer}");
    assert_eq!(project.lock_mode(), "unattended");
    std::thread::sleep(Duration::from_millis(200));
    project.spend(5000);

    // The lane reports on the snapshot the switch rewrote, then the pass that
    // would start the next one meets the ceiling instead.
    let (status, stderr) = project.report_from_lane("ta");
    assert_eq!(status.trim(), "0", "report failed:\n{stderr}");
    project.wait_for("the ceiling to stop the run", |p| {
        p.dispatch_text()
            .contains("spent 5000 output tokens against a max_output_tokens of 1000")
    });
    assert_eq!(project.started().len(), 1, "a second lane started");
}

/// A reload that turns unattended off lifts the ceiling from the next pass:
/// spend that would have stopped an unattended run starts the next lane.
#[cfg(unix)]
#[test]
fn a_reload_that_turns_unattended_off_lifts_the_ceiling() {
    let mut project = Project::new();
    project.set_up();
    project.run(&["config", "set", "unattended.max_output_tokens", "1000"]);
    project.run(&[
        "config",
        "set",
        "unattended.blocked_model",
        "claude-sonnet-5-5",
    ]);
    project.run(&["config", "set", "unattended.enabled", "true"]);
    project.start_dispatcher();
    project.wait_for("the first lane to start", |p| p.started().len() == 1);
    assert_eq!(project.lock_mode(), "unattended");

    project.run(&["config", "set", "unattended.enabled", "false"]);
    let answer = project.reload("3-1");
    assert!(answer.contains("Loaded"), "reload was not taken:\n{answer}");
    assert_eq!(project.lock_mode(), "attended");
    project.spend(5000);

    let (status, stderr) = project.report_from_lane("ta");
    assert_eq!(status.trim(), "0", "report failed:\n{stderr}");
    project.wait_for("the next lane to start", |p| p.started().len() == 2);
    assert!(
        !project
            .dispatch_text()
            .contains("max_output_tokens reached"),
        "the ceiling still applied:\n{}",
        project.dispatch_text()
    );
}
