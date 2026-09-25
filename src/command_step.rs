//! Running a plain command as a pipeline step.
//!
//! A `kind: command` step is the pipeline's way of saying "and then run this",
//! where *this* is any command line: a build, a test suite, a deploy script, a
//! benchmark. No model, no prompt, no worker slot.
//!
//! ## A run is a process, watched across passes
//!
//! The same shape [`crate::headless`] gives a lane, and for the same reason: a
//! dispatch pass is a short reconciliation over the whole queue, so a command
//! that takes four minutes cannot be something a pass waits on — every other
//! task in the project would wait with it. So a run is spawned detached, and
//! each later pass asks
//! the same two questions about it that it asks of a lane: is it still going,
//! and what did it end with.
//!
//! Both answers are files. The wrapper shell writes its own pid before starting
//! the command and the command's exit code after it, which makes liveness a
//! question about the filesystem rather than about a child this process would
//! have to have stayed alive to hold.
//!
//! ## Nothing confines it
//!
//! Nothing confines an agent lane either, since the sandbox was retired — but
//! the distinction is still worth stating. It is written by a person, in a file
//! only people write, and it runs with the privileges of whoever started the
//! dispatcher: a Makefile target, not a lane. Nothing routes it through
//! `spoolway report` at all, so a `run:` line is trusted with what a lane is
//! not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Where a run's pid, exit code and log live, under the project's home
/// directory — see [`crate::repo::Repo::commands_dir`].
pub(crate) const RUN_DIR: &str = "commands";

/// What a command step's run is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Nothing has been started for this task and step.
    Fresh,
    /// Spawned, and still going.
    Running,
    /// Over, with this exit code. Anything but zero is a failure, which is the
    /// shell's own convention and the only one a `run:` line can be written
    /// against.
    Exited(i32),
    /// Gone without leaving an exit code, so there is no verdict to route on.
    ///
    /// The wrapper writes its code from a trap on `EXIT`, which fires however
    /// the command itself ends — a crash, a `kill`, a script calling `exit`.
    /// Getting here means the wrapper never ran that trap, and the only ways
    /// that happens are the whole process group taking a `SIGKILL` or the
    /// machine going down under it.
    ///
    /// Neither is the command's verdict, and reading them as one is expensive.
    /// A dispatcher stopping at its output ceiling kills in-flight commands
    /// before it releases their worktrees; every one of those used to surface
    /// as `Exited(1)`, which routed the task down its `on_fail` edge. Three
    /// lanes bounced that way in one run over suites whose own logs recorded
    /// nothing failing, and one of them cost seven agent turns.
    Interrupted,
}

/// Runs owned by one project.
#[derive(Debug, Clone)]
pub struct Runs {
    dir: PathBuf,
    /// The pid and exit file protocol itself — see
    /// [`crate::runfiles::RunFiles`], shared with [`crate::headless::Headless`]
    /// which tracks a lane's turn the same way under its own directory.
    files: crate::runfiles::RunFiles,
}

impl Runs {
    /// `dir` is where a run's pid, exit code and log live —
    /// [`crate::repo::Repo::commands_dir`] for every real caller.
    pub fn new(dir: &Path) -> Runs {
        Runs {
            dir: dir.to_path_buf(),
            files: crate::runfiles::RunFiles::new(dir.to_path_buf()),
        }
    }

    /// What a run is addressed by afterwards: `<task> · <step>`, the same
    /// shape a lane name has, so a person reading the project's own
    /// `commands/` sees the names they already know from `headless/`.
    pub fn key(step: &str, task: &str) -> String {
        crate::mux::lane_name(step, task)
    }

    /// Which of `peers` holds a `serial: true` step right now: the first
    /// whose own run of `step` is still going, or `None` when the step is
    /// free.
    ///
    /// Asked of the run files, never of where a peer's task is: a
    /// `background: true` run's task has walked on to a later step while its
    /// run still holds this one. And asked fresh, not off a pass's one
    /// directory read, so a run started earlier in the same pass —
    /// [`Runs::start`] and a paned run alike wait for the wrapper's pid
    /// before answering — already holds the step for the next task the pass
    /// reaches. An exited or interrupted run holds nothing.
    ///
    /// `peers` are the other tasks on this step's own pipeline: a key names
    /// no pipeline, so a step of the same id elsewhere is left out by the
    /// caller, not here.
    pub fn serial_holder<'a>(
        &self,
        step: &str,
        peers: impl IntoIterator<Item = &'a str>,
    ) -> Option<&'a str> {
        peers
            .into_iter()
            .find(|peer| self.state(&Runs::key(step, peer)) == RunState::Running)
    }

    /// Everything the command wrote, both streams, kept after the run is
    /// forgotten — for a background run with no `on_fail` it is the *only*
    /// account of what happened, since nothing routes on its outcome.
    pub fn log_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.log"))
    }

    /// The run before this one, kept so a step that comes back round does not
    /// destroy the account of why it came back.
    ///
    /// A step whose `on_fail` routes to a lane and back — `gate` failing into
    /// `e2e`, most often — used to overwrite its own log the moment it ran
    /// again, so by the time anyone read it the only output on disk was from
    /// the attempt they were not asking about. One generation back is the
    /// whole of what this keeps: it answers "why did the last one fail" and
    /// costs one file per key, where keeping every run would grow without a
    /// bound and need sweeping.
    pub fn prev_log_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.prev.log"))
    }

    fn pid_path(&self, key: &str) -> PathBuf {
        self.files.pid_path(key)
    }

    /// Where the pane a run landed in is recorded, if it landed in one at
    /// all — see [`Runs::record_pane`].
    fn pane_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.pane"))
    }

    /// Written by the wrapper as its last act, so its presence means the run is
    /// over however the command ended. It is also what makes liveness safe
    /// against pid reuse: a recycled pid would read as alive, but a finished run
    /// has already said otherwise here.
    fn exit_path(&self, key: &str) -> PathBuf {
        self.files.exit_path(key)
    }

    pub fn state(&self, key: &str) -> RunState {
        if let Some(code) = self.files.read_exit_code(key) {
            return RunState::Exited(code);
        }
        match self.read_pid(key) {
            None => RunState::Fresh,
            Some(pid) if !crate::headless::alive(pid) => self.what_a_dead_wrapper_left(key),
            Some(_) => RunState::Running,
        }
    }

    /// Read the exit file a second time, now that the wrapper is known to be
    /// gone, and only call the run interrupted if it is still not there.
    ///
    /// The two reads above are two separate moments, and the run goes on
    /// living between them. The wrapper writes its code and *then* dies — the
    /// trap fires before the shell is gone — so a wrapper that has died has
    /// already written whatever it was going to write. A single read that
    /// arrived a moment too early therefore proves nothing on its own: the
    /// code can land, and the process go away, entirely inside the gap
    /// between the read of the exit file and the read of `/proc`, which is
    /// what a loaded machine widens. `a_kept_log_is_not_mistaken_for_a_run`
    /// caught exactly that, reading `Interrupted` off a run that had exited
    /// 0, and the cost in the product is worse than a red test: the dispatcher
    /// answers `Interrupted` by forgetting the run and starting the command
    /// over — so a command step that had passed was silently re-run, and its
    /// log rolled aside, at random and only under load.
    ///
    /// Re-reading settles it, because after death the file no longer changes.
    /// Nothing there still means what it always meant: killed, or the machine
    /// went down under it, with no code to route on — which is a different
    /// thing from a code that says the command failed. See
    /// [`RunState::Interrupted`].
    fn what_a_dead_wrapper_left(&self, key: &str) -> RunState {
        match self.files.read_exit_code(key) {
            Some(code) => RunState::Exited(code),
            None => RunState::Interrupted,
        }
    }

    /// How long this run has been going, or `None` if it never started.
    ///
    /// Read off the pid file's own timestamp rather than kept in a record: the
    /// wrapper writes that file as its first act, so the filesystem already
    /// knows when the run began and there is no second place for it to
    /// disagree. It has to be on disk at all because a dispatch pass is its own
    /// process — anything held in memory would not survive to the pass that
    /// needs it.
    pub fn elapsed(&self, key: &str) -> Option<Duration> {
        let started = std::fs::metadata(self.pid_path(key))
            .ok()?
            .modified()
            .ok()?;
        // A clock that went backwards is not evidence that a run has overrun.
        Some(started.elapsed().unwrap_or(Duration::ZERO))
    }

    /// The wrapper's own pid, which is the process group everything the
    /// command started belongs to.
    pub fn read_pid(&self, key: &str) -> Option<u32> {
        self.files.read_pid(key)
    }

    /// Clear the way for a fresh run: make room for it, drop whatever a
    /// previous arrival at this step left behind, and roll its log aside.
    ///
    /// Shared by [`Runs::start`] and [`Runs::script_for_pane`] — same pid
    /// file, same exit trap, same roll-aside either way, and differing only
    /// in where the command's output ends up.
    fn prepare(&self, key: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        // A stale exit code from an earlier arrival at this step would make the
        // run about to start look finished before it had written a line. The
        // `?` is the point: a run that cannot be given a clean slate must not
        // be spawned into a dirty one, because the code it would be read by is
        // the one the last arrival left.
        self.files.clear(key)?;
        // Rolled aside rather than deleted — see [`Runs::prev_log_path`]. The
        // rename also leaves no log at `log_path`, which is what the wrapper's
        // `>>` append below needs: a fresh run must not open onto the tail of
        // the last one.
        self.roll_log_aside(key);
        // Created here, before anything is spawned, rather than left for the
        // wrapper's own `>>` redirect to bring into existence: a wrapper that
        // never got as far as running at all — no `sh` on PATH, a backend
        // that dropped the script on the floor — used to leave no file behind,
        // so [`Runs::await_started`]'s error named a path that did not exist.
        std::fs::File::create(self.log_path(key))
            .with_context(|| format!("creating {}", self.log_path(key).display()))?;
        Ok(())
    }

    /// Move the last run's log out of the way.
    ///
    /// One `rename` is enough: a file renames whoever has it open, and the
    /// wrapper closes its own log the moment the group behind the pipe ends,
    /// so nothing is still writing to `from` by the time a fresh `start` or
    /// `script_for_pane` gets here. Silent about a failure — including no log
    /// to roll aside at all, the first arrival at this step — because nothing
    /// downstream needs the old log to have moved; `prev_log_path` reading
    /// stale or absent is exactly the answer a caller with no prior run gets.
    fn roll_log_aside(&self, key: &str) {
        let from = self.log_path(key);
        let to = self.prev_log_path(key);
        let _ = std::fs::rename(&from, &to);
    }

    /// The pid write, the environment, the `run:` line itself, and the exit
    /// report every wrapper closes with — shared by [`Runs::start`]'s
    /// detached wrapper and [`Runs::script_for_pane`]'s piped one.
    fn wrapper_body(&self, key: &str, run: &str, env: &BTreeMap<String, String>) -> String {
        let pid_path = crate::platform::quote(&self.pid_path(key).display().to_string());
        let exit_path = crate::platform::quote(&self.exit_path(key).display().to_string());
        let mut body = String::new();
        body.push_str(&format!("echo $$ >{pid_path}\n"));
        // The exit code is written from a trap rather than by a line after
        // the command, because `run: ./deploy.sh || exit 1` — or anything
        // else that ends the shell itself — would never reach that line, and
        // the run would read as one that died without a code. The trap fires
        // on every way out. The path goes through a variable so the two
        // quoting styles never have to nest.
        body.push_str(&format!(
            "__spoolway_exit={exit_path}\ntrap 'echo $? >\"$__spoolway_exit\"' EXIT\n"
        ));
        if !env.is_empty() {
            body.push_str(&crate::platform::env_export(env));
            body.push('\n');
        }
        body.push_str(run);
        body.push('\n');
        body
    }

    /// Spawn the command, detached, and return once its wrapper has said where
    /// it is.
    pub fn start(
        &self,
        key: &str,
        run: &str,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<u32> {
        self.prepare(key)?;

        // The `run:` line as written, not as parsed: it goes to the shell whole,
        // so a pipe, an `&&`, a redirect and a glob all mean what they mean at a
        // prompt. That is the whole contract of the key.
        let body = self.wrapper_body(key, run, env);
        let log_path = crate::platform::quote(&self.log_path(key).display().to_string());
        // stdin from /dev/null: a command step is non-interactive by
        // construction, and one that reads stdin should find end-of-file
        // rather than block forever on a terminal nobody is attached to.
        let script = format!("exec </dev/null >>{log_path} 2>&1\n{body}");

        let spawned = spawn_wrapper(&script, cwd)?;
        crate::headless::reap_when_it_ends(spawned);

        self.await_started(key)
    }

    /// The wrapper as it runs inside a pane, rather than detached: the whole
    /// group is piped to `tee`, so a person watching the pane sees every line
    /// as plain text while the log gets the same lines, and the `EXIT` trap
    /// still reads the command's own status — no `PIPESTATUS` needed, since
    /// the group on the left of the pipe is what the trap watches.
    ///
    /// The body runs inside its own `sh -c`, not a bare `{ ... }` group: a
    /// brace group on the left of a pipe still runs in a subshell, but `$$`
    /// there is inherited from the shell that forked it rather than computed
    /// fresh, so the pid `wrapper_body`'s first line writes would name the
    /// pane's own long-lived shell — which outlives every command ever run
    /// in it — rather than this run. `sh -c` execs a genuinely new process
    /// image, whose own `$$` is its own real pid: the same one that ends
    /// when the pipeline's first process does, and the one a job-controlled
    /// shell makes the leader of the pipeline's own process group.
    ///
    /// Answers with the script text alone; running it is [`crate::mux::Mux::run_in_pane`]'s
    /// job, since only the backend can put it somewhere a person can watch.
    /// The caller still confirms the run actually started with
    /// [`Runs::await_started`], the same as [`Runs::start`] does for itself.
    pub fn script_for_pane(
        &self,
        key: &str,
        run: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<String> {
        self.prepare(key)?;

        let body = self.wrapper_body(key, run, env);
        let log_path = crate::platform::quote(&self.log_path(key).display().to_string());
        // stdin from /dev/null, same as the detached wrapper: a command step
        // is non-interactive by construction. Nothing to redirect stdout or
        // stderr to here — the whole group is piped to `tee` below instead.
        //
        // The body is handed to the nested `sh -c` as one quoted argument,
        // not typed into the pane's own shell as syntax: this is text, going
        // through a variable, and quoting it is what keeps it from being
        // touched by the pane's shell before the nested one ever sees it.
        let inner = crate::platform::quote(&format!("exec </dev/null\n{body}"));
        Ok(format!("sh -c {inner} 2>&1 | tee -a {log_path}\n"))
    }

    /// Wait for the wrapper's pid file to appear — the one thing that says a
    /// run actually got going, whether it was spawned detached by
    /// [`Runs::start`] or handed to a pane by [`crate::mux::Mux::run_in_pane`].
    /// A wrapper that never wrote one failed before it got that far: no `sh`
    /// on PATH, a backend that dropped the script on the floor.
    pub fn await_started(&self, key: &str) -> Result<u32> {
        match crate::headless::await_pid_file(|| self.read_pid(key)) {
            Some(pid) => Ok(pid),
            None => bail!(
                "`{key}` was spawned but never reported a pid — see {}",
                self.log_path(key).display()
            ),
        }
    }

    /// The pane a run landed in, if [`Runs::record_pane`] ever recorded one
    /// for this key. `None` for a headless run, and for a paned one whose
    /// pane has already been closed and forgotten.
    pub fn pane(&self, key: &str) -> Option<String> {
        let raw = std::fs::read_to_string(self.pane_path(key)).ok()?;
        let pane = raw.trim();
        (!pane.is_empty()).then(|| pane.to_string())
    }

    /// Record which pane a run landed in, so a later pass — or cleanup — can
    /// find it again without asking the multiplexer to list everything.
    pub fn record_pane(&self, key: &str, pane_id: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.pane_path(key), pane_id)?;
        Ok(())
    }

    /// Drop the pane record without touching the pane itself — for a caller
    /// that has already closed it, or never opened one to begin with.
    pub fn forget_pane(&self, key: &str) {
        let _ = std::fs::remove_file(self.pane_path(key));
    }

    /// End a run and everything under it. Silent about a run that is already
    /// over, which is the ordinary case.
    ///
    /// Signals unconditionally, including a run that has already finished.
    /// A pid here addresses a process *group*, [`crate::headless::kill_group`]
    /// refuses a group with nothing left in it, and reaching a group whose
    /// leader has already exited is the reason that function exists — a
    /// `run:` line that backgrounded a server and then returned leaves
    /// exactly that shape behind, and it is the leader's own exit that makes
    /// it invisible to every other check. Membership is the guard, and it is
    /// also what makes a recycled pid safe: a reused number is only a group
    /// again if something new leads one.
    pub fn stop(&self, key: &str) {
        if let Some(pid) = self.read_pid(key) {
            crate::headless::kill_group(pid);
        }
        // Nowhere to report a clearing that failed, and nothing that needs it
        // to have succeeded: this run is over either way, and the next arrival
        // at this key goes through [`Runs::prepare`], which does not carry on
        // past it.
        let _ = self.files.clear(key);
    }

    /// Forget a finished run's bookkeeping, keeping its log.
    ///
    /// Called once the exit code has been routed on: without it a task that
    /// comes back round to the same step would read the last arrival's code and
    /// route on it without running anything.
    ///
    /// That sentence describes a real failure rather than a hypothetical one,
    /// which is why this answers with a result instead of swallowing one. The
    /// next arrival does not necessarily start a run: a dispatch pass reads
    /// the state first, so a code left lying here is routed on before
    /// [`Runs::prepare`] is ever reached and gets a chance to refuse. Forget
    /// has to have actually forgotten.
    pub fn forget(&self, key: &str) -> Result<()> {
        self.files.clear(key)
    }

    /// Every run belonging to one task, whatever step started it.
    ///
    /// Cleanup needs this and cannot ask the pipeline: a background run started
    /// four steps ago is still going, and the step that started it is not where
    /// the task is now.
    ///
    /// A `.pane` file answers here too, not only a `.pid` — a paned run a
    /// step's own `timeout:` stopped already dropped its pid the moment
    /// `stop()` cleared it, but the pane that run left standing, for the
    /// `Fresh` arm to replace on a later arrival, is still this task's to
    /// take at cleanup if that arrival never comes, and it has no other file
    /// to be found by.
    pub fn keys_for_task(&self, task: &str) -> Vec<String> {
        // A key is `<task> · <step>` — see `Runs::key` — so it is this task's
        // if it starts with its id and the separator, never by a bare prefix
        // match: `"demo-two"` must not answer for `"demo"`.
        let prefix = format!("{task} · ");
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let keys: std::collections::BTreeSet<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|e| e == "pid" || e == "pane")
            })
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
            })
            .filter(|key| key.starts_with(&prefix))
            .collect();
        keys.into_iter().collect()
    }

    /// [`Runs::keys_for_task`], for every task at once, off one directory
    /// read rather than one per task.
    ///
    /// A pass used to call `keys_for_task` in a loop over the whole queue —
    /// [`crate::dispatch::Dispatcher::sweep_anchor_tabs`] and the reap done
    /// ahead of routing both did — which cost one `read_dir` per task on
    /// every pass, and every tick once ticks ran independently of a probe's
    /// own clock. This reads the directory exactly once and hands back each
    /// task's own keys, so a caller wanting more than one task's worth pays
    /// for the walk a single time.
    pub fn keys_by_task(&self) -> std::collections::HashMap<String, Vec<String>> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return std::collections::HashMap::new();
        };
        let mut by_task: std::collections::HashMap<String, std::collections::BTreeSet<String>> =
            std::collections::HashMap::new();
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "pid" || e == "pane") {
                continue;
            }
            let Some(key) = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
            else {
                continue;
            };
            // A key is `<task> · <step>` — see `Runs::key` — so the task it
            // belongs to is everything before the first separator.
            let Some((task, _)) = key.split_once(" · ") else {
                continue;
            };
            by_task.entry(task.to_string()).or_default().insert(key);
        }
        by_task
            .into_iter()
            .map(|(task, keys)| (task, keys.into_iter().collect()))
            .collect()
    }

    /// Delete every run file this directory holds for `task`, across every
    /// step and whatever extension — logs, pids, exit codes, pane records and
    /// a hook's `.out`/`.failed` markers alike.
    ///
    /// Called when a task is archived. Nothing routes on a run of a task that
    /// has left the queue, and left in place these files accumulate for the
    /// life of the project — for `tracking/` that also means
    /// [`crate::tracking::failure_count`] goes on counting a long-archived
    /// task's failed hook (review finding 64). Matched on the full file name
    /// against the `<task> · ` prefix, so `"demo-two · x.log"` is never taken
    /// for `"demo"`'s.
    pub fn reclaim_task(&self, task: &str) {
        let prefix = format!("{task} · ");
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Spawn a wrapper script detached, so it outlives the pass that started it.
///
/// `libc::setsid()`, called in the child between fork and exec — that is
/// what `setsid(1)` itself does, and calling the syscall directly means this
/// does not depend on that binary being on PATH, which macOS does not ship
/// and Homebrew's keg-only `util-linux` does not fix.
fn spawn_wrapper(script: &str, cwd: &Path) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(script);
    // SAFETY: `setsid()` only detaches the child into its own session; it
    // touches nothing this process holds, and runs after `fork` so a failure
    // in it cannot affect this process either.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("could not start a command step")
}

// Every test below starts a real run: `sh -c` under `libc::setsid()` — see
// `spawn_wrapper` and `Runs::wrapper_body`.
#[cfg(test)]
mod tests {
    use super::*;

    /// A `run:` line that writes `msg` to stderr and exits with `code`.
    fn write_stderr_then_exit(msg: &str, code: i32) -> String {
        format!("echo {msg} >&2; exit {code}")
    }

    /// A `run:` line that takes the `both` branch of a conditional and never
    /// reaches `neither` — proof the whole line reached the shell as syntax,
    /// not as an argv.
    fn conditional_taking_the_first_branch() -> String {
        "true && echo both || echo neither".to_string()
    }

    struct Fixture {
        root: PathBuf,
        runs: Runs,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root = crate::scratch::root(&format!("command-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Fixture {
                runs: Runs::new(&root.join("commands")),
                root,
            }
        }

        fn start(&self, key: &str, run: &str) -> u32 {
            self.runs
                .start(key, run, &self.root, &BTreeMap::new())
                .unwrap()
        }

        /// Stands in for what [`crate::mux::Mux::run_in_pane`] would do with
        /// the script — a real backend hands it to a live pane; this just
        /// runs it plainly, which is enough to prove the script itself is
        /// right, since nothing about pid, exit code or log depends on what
        /// is showing it to a person.
        fn start_in_pane(&self, key: &str, run: &str) -> u32 {
            let script = self
                .runs
                .script_for_pane(key, run, &BTreeMap::new())
                .unwrap();
            // A real pane is exactly where this output belongs; here it would
            // only be noise on the test's own stdout. `shell_command` rather
            // than a hardcoded `sh -c`, so this runs the script the way a
            // real pane would on either platform.
            let mut command = crate::platform::shell_command(&script);
            let child = command
                .current_dir(&self.root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            crate::headless::reap_when_it_ends(child);
            self.runs.await_started(key).unwrap()
        }

        /// Wait for the run to be over, so an assertion about an exit code is
        /// never a race with a process that is still starting.
        fn settle(&self, key: &str) -> RunState {
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(20) {
                match self.runs.state(key) {
                    RunState::Running | RunState::Fresh => {
                        std::thread::sleep(std::time::Duration::from_millis(25))
                    }
                    done => return done,
                }
            }
            panic!("`{key}` never finished");
        }

        /// A paned run's log, polled rather than read the instant [`settle`]
        /// says the run is over.
        ///
        /// `settle` only waits for the `.exit` marker the wrapper's own trap
        /// writes when its exec group ends — and for the piped wrapper
        /// [`script_for_pane`] builds, that group's output still has to pass
        /// through `tee` before it lands in the log file, which is a separate
        /// process downstream of the pipe rather than something the trap
        /// waits on. A read straight after `settle` can win that race and
        /// find the file before `tee` has flushed the last of it — rare, and
        /// only under load, but the failure this test actually hit.
        fn log_containing(&self, key: &str, expect: &str) -> String {
            let path = self.runs.log_path(key);
            let started = std::time::Instant::now();
            loop {
                let log = std::fs::read_to_string(&path).unwrap_or_default();
                if log.contains(expect) || started.elapsed() > std::time::Duration::from_secs(5) {
                    return log;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The whole life of a blocking run, which is what the dispatcher reads on
    /// three successive passes.
    #[test]
    fn a_run_is_fresh_then_running_then_exited() {
        let f = Fixture::new("lifecycle");
        assert_eq!(f.runs.state("build-demo"), RunState::Fresh);

        // The run waits on a file this test creates rather than on a fixed
        // `sleep`. A timed sleep makes `Running` a race against the clock:
        // under load the gap between `start` returning and the read below can
        // outlast the sleep, so the run has already reached `Exited(0)` and
        // the state this test exists to observe never gets seen — the failure
        // it actually hit. A gate the test opens itself cannot close early,
        // which makes `Running` a fact rather than a guess about timing.
        let gate = f.root.join("release");
        f.start(
            "build-demo",
            &format!(
                "while [ ! -f '{}' ]; do sleep 0.05; done; echo built",
                gate.display()
            ),
        );
        assert_eq!(f.runs.state("build-demo"), RunState::Running);

        std::fs::write(&gate, "go").unwrap();
        assert_eq!(f.settle("build-demo"), RunState::Exited(0));
        let log = std::fs::read_to_string(f.runs.log_path("build-demo")).unwrap();
        assert!(log.contains("built"), "the run's output is its log: {log}");
    }

    /// `await_started`'s own error names `log_path` as a file that exists —
    /// so `prepare` has to create it before anything is spawned, rather than
    /// leaving it for the wrapper's own `>>` redirect: a wrapper that never
    /// ran at all would otherwise leave no file for that error to point at.
    #[test]
    fn prepare_creates_the_log_before_anything_is_spawned() {
        let f = Fixture::new("prepare-creates-log");
        f.runs.prepare("build-demo").unwrap();
        assert!(f.runs.log_path("build-demo").exists());
    }

    /// The routing decision itself: the exit code reaches the dispatcher intact,
    /// because `on_pass` and `on_fail` are picked from nothing else.
    #[test]
    fn a_failing_command_reports_its_code() {
        let f = Fixture::new("exit-code");
        f.start("build-demo", &write_stderr_then_exit("nope", 3));
        assert_eq!(f.settle("build-demo"), RunState::Exited(3));
        let log = std::fs::read_to_string(f.runs.log_path("build-demo")).unwrap();
        assert!(log.contains("nope"), "stderr belongs in the log too: {log}");
    }

    /// A `run:` line that ends the shell itself still records what it ended
    /// with. `exit` is ordinary in a deploy script, and a line after the command
    /// would never run — the code would be lost and the step would route on a
    /// failure it invented.
    #[test]
    fn a_command_that_exits_the_shell_still_reports_its_code() {
        let f = Fixture::new("explicit-exit");
        f.start("deploy-demo", "echo leaving; exit 4");
        assert_eq!(f.settle("deploy-demo"), RunState::Exited(4));
        assert!(
            std::fs::read_to_string(f.runs.log_path("deploy-demo"))
                .unwrap()
                .contains("leaving")
        );
    }

    /// The `run:` line reaches a shell whole. A step that says `a && b` means
    /// the shell's `&&`, not a command called `a` with two odd arguments.
    #[test]
    fn a_run_line_is_shell_syntax_not_an_argv() {
        let f = Fixture::new("shell-syntax");
        f.start("build-demo", &conditional_taking_the_first_branch());
        assert_eq!(f.settle("build-demo"), RunState::Exited(0));
        let log = std::fs::read_to_string(f.runs.log_path("build-demo")).unwrap();
        assert!(log.contains("both"), "{log}");
        assert!(!log.contains("neither"), "{log}");
    }

    /// The piped wrapper a pane runs is a different script from the detached
    /// one, but the same contract: the log holds every line, and the exit
    /// code still comes from the wrapper's own `EXIT` trap.
    #[test]
    fn a_paned_run_reports_its_code_the_same_as_a_detached_one() {
        let f = Fixture::new("pane-lifecycle");
        f.start_in_pane("build-demo", "echo built-in-a-pane; exit 4");
        assert_eq!(f.settle("build-demo"), RunState::Exited(4));
        let log = f.log_containing("build-demo", "built-in-a-pane");
        assert!(log.contains("built-in-a-pane"), "{log}");
    }

    /// A `run:` line that ends the shell itself is exactly as ordinary
    /// piped as detached — the trap fires on every way out of the group,
    /// whatever is downstream of it.
    #[test]
    fn a_paned_run_that_exits_the_shell_still_reports_its_code() {
        let f = Fixture::new("pane-explicit-exit");
        f.start_in_pane("deploy-demo", "echo leaving; exit 4");
        assert_eq!(f.settle("deploy-demo"), RunState::Exited(4));
    }

    /// Recorded, read back, and dropped — the whole of what a caller needs to
    /// find a run's pane again, or to say it has none any more.
    #[test]
    fn pane_bookkeeping_round_trips() {
        let f = Fixture::new("pane-bookkeeping");
        assert_eq!(f.runs.pane("demo · build"), None);

        f.runs.record_pane("demo · build", "w1:p9").unwrap();
        assert_eq!(f.runs.pane("demo · build"), Some("w1:p9".to_string()));

        f.runs.forget_pane("demo · build");
        assert_eq!(f.runs.pane("demo · build"), None);
    }

    /// A run a step's own `timeout:` stopped has its pid cleared straight
    /// away, but the pane it left standing for the `Fresh` arm to replace is
    /// still this task's to take at cleanup if that arrival never comes — so
    /// `keys_for_task` has to find it by the `.pane` file alone, with no
    /// `.pid` beside it any more.
    #[test]
    fn keys_for_task_finds_a_run_whose_pane_outlived_its_pid() {
        let f = Fixture::new("pane-only-key");
        f.start("demo · build", "exit 1");
        assert_eq!(f.settle("demo · build"), RunState::Exited(1));
        f.runs.record_pane("demo · build", "w1:p9").unwrap();
        f.runs.forget("demo · build").unwrap();

        assert_eq!(f.runs.state("demo · build"), RunState::Fresh);
        assert_eq!(
            f.runs.keys_for_task("demo"),
            vec!["demo · build".to_string()],
            "a pane left standing behind a forgotten pid is still this task's"
        );
    }

    /// A command runs where the task's work is. Anything else would have a build
    /// step building the wrong checkout.
    #[test]
    fn a_run_happens_in_the_directory_it_was_given() {
        let f = Fixture::new("cwd");
        let worktree = f.root.join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("marker.txt"), "here").unwrap();

        f.runs
            .start("build-demo", "cat marker.txt", &worktree, &BTreeMap::new())
            .unwrap();
        assert_eq!(f.settle("build-demo"), RunState::Exited(0));
        assert!(
            std::fs::read_to_string(f.runs.log_path("build-demo"))
                .unwrap()
                .contains("here")
        );
    }

    /// The environment is how a script knows which task it is running for —
    /// the same variables a lane is given, from the same place.
    #[test]
    fn a_runs_environment_reaches_the_command() {
        let f = Fixture::new("env");
        let env = BTreeMap::from([
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
            ("SPOOLWAY_STEP".to_string(), "build".to_string()),
        ]);
        let echo_env = r#"echo "TASK=$SPOOLWAY_TASK STEP=$SPOOLWAY_STEP""#;
        f.runs.start("build-demo", echo_env, &f.root, &env).unwrap();
        assert_eq!(f.settle("build-demo"), RunState::Exited(0));
        assert!(
            std::fs::read_to_string(f.runs.log_path("build-demo"))
                .unwrap()
                .contains("TASK=add-endpoint STEP=build")
        );
    }

    /// A task that comes back round to the same step must run it again. Reading
    /// the last arrival's exit code would route the second visit on the first
    /// one's result, having run nothing at all.
    #[test]
    fn a_forgotten_run_is_fresh_again() {
        let f = Fixture::new("forget");
        f.start("build-demo", "exit 7");
        assert_eq!(f.settle("build-demo"), RunState::Exited(7));

        f.runs.forget("build-demo").unwrap();
        assert_eq!(f.runs.state("build-demo"), RunState::Fresh);
        // The log outlives the bookkeeping: it is the account of what the run
        // did, and the next arrival has not happened yet.
        assert!(f.runs.log_path("build-demo").exists());
    }

    /// The whole point of the roll-aside: a gate that fails into a lane and is
    /// re-run by it must not have destroyed the output that says why it failed
    /// the first time. Without this the second arrival is the only account
    /// left, and it is the run nobody is asking about.
    #[test]
    fn a_second_run_keeps_the_first_ones_log() {
        let f = Fixture::new("prev-log");
        f.start("build-demo", "echo first-run-said-this; exit 3");
        assert_eq!(
            f.settle("build-demo"),
            RunState::Exited(3),
            "first run's log: {:?}",
            std::fs::read_to_string(f.runs.log_path("build-demo"))
        );
        // Wait for the first run's own line to land before rolling it aside.
        // `settle` waits on the `.exit` marker the wrapper's exec group
        // writes, which the trap fires before the shell's own redirect onto
        // the log is necessarily flushed and closed, so the line can still be
        // in flight here, and the assertion below is about that line being
        // in the *kept* log. The handle that redirect is still holding is a
        // separate problem, and one this test deliberately leaves to the
        // product: re-running the instant a step exits is what a lane really
        // does, and [`Runs::roll_log_aside`] is what has to survive it.
        f.log_containing("build-demo", "first-run-said-this");
        f.runs.forget("build-demo").unwrap();

        f.start("build-demo", "echo second-run-said-this");
        // The log goes into both failure messages above and below: this test
        // fired on CI for a run whose `run:` line was fine, and the log is
        // the only thing that would have said what actually went wrong.
        assert_eq!(
            f.settle("build-demo"),
            RunState::Exited(0),
            "second run's log: {:?}",
            std::fs::read_to_string(f.runs.log_path("build-demo"))
        );

        let now = std::fs::read_to_string(f.runs.log_path("build-demo")).unwrap();
        let before = std::fs::read_to_string(f.runs.prev_log_path("build-demo")).unwrap();
        assert!(before.contains("first-run-said-this"), "{before}");
        assert!(now.contains("second-run-said-this"), "{now}");
        // The fresh run appends to its own file, not onto the tail of the last
        // one — the rename is what leaves `log_path` empty for it.
        assert!(!now.contains("first-run-said-this"), "{now}");
    }

    /// The roll-aside writes a second file per key, and cleanup finds a task's
    /// runs by reading the directory. A `.prev.log` that answered as a key
    /// would have cleanup stopping runs that do not exist.
    #[test]
    fn a_kept_log_is_not_mistaken_for_a_run() {
        let f = Fixture::new("prev-keys");
        f.start("demo · gate", "exit 1");
        assert_eq!(f.settle("demo · gate"), RunState::Exited(1));
        f.runs.forget("demo · gate").unwrap();
        f.start("demo · gate", "exit 0");
        assert_eq!(f.settle("demo · gate"), RunState::Exited(0));

        assert!(f.runs.prev_log_path("demo · gate").exists());
        assert_eq!(
            f.runs.keys_for_task("demo"),
            vec!["demo · gate".to_string()]
        );
    }

    /// A background run is the one a task walks away from, so cleanup has to be
    /// able to find it without knowing which step started it.
    #[test]
    fn a_tasks_runs_are_findable_by_the_task_alone() {
        let f = Fixture::new("by-task");
        f.start("demo · bench", "sleep 30");
        f.start("demo · build", "sleep 30");
        f.start("other · build", "sleep 30");

        assert_eq!(
            f.runs.keys_for_task("demo"),
            vec!["demo · bench".to_string(), "demo · build".to_string()],
            "a task's own runs, and nobody else's"
        );

        for key in f.runs.keys_for_task("demo") {
            f.runs.stop(&key);
        }
        f.runs.stop("other · build");
    }

    /// The batch form: the same split by task, off one directory read
    /// rather than one per task.
    #[test]
    fn keys_by_task_splits_one_directory_read_by_task() {
        let f = Fixture::new("by-task-batch");
        f.start("demo · bench", "sleep 30");
        f.start("demo · build", "sleep 30");
        f.start("other · build", "sleep 30");

        let by_task = f.runs.keys_by_task();
        assert_eq!(
            by_task.get("demo").cloned().unwrap_or_default(),
            vec!["demo · bench".to_string(), "demo · build".to_string()],
            "grouped exactly as keys_for_task would answer for this one task"
        );
        assert_eq!(
            by_task.get("other").cloned().unwrap_or_default(),
            vec!["other · build".to_string()]
        );

        for keys in by_task.into_values() {
            for key in keys {
                f.runs.stop(&key);
            }
        }
    }

    /// Stopping a run has to take the command with it, not just the wrapper that
    /// started it — the wrapper is a process-group leader precisely so this can
    /// reach through it.
    #[test]
    fn stopping_a_run_kills_the_command_under_it() {
        let f = Fixture::new("stop");
        let pid = f.start("bench-demo", "sleep 60");
        assert_eq!(f.runs.state("bench-demo"), RunState::Running);

        f.runs.stop("bench-demo");
        assert!(
            !crate::headless::alive(pid),
            "the command survived its run being stopped"
        );
        assert_eq!(f.runs.state("bench-demo"), RunState::Fresh);
    }

    /// A `run:` line that backgrounds a server and returns leaves that server
    /// running in the group under a leader that has already exited — exactly
    /// the shape [`crate::headless::kill_group`]'s membership check exists to
    /// reach. `Runs::stop` must signal the group whether or not its own
    /// wrapper is still the one running, so a person who calls `stop` after
    /// the run has already finished still takes the backgrounded child down
    /// with it.
    #[test]
    fn stop_reaches_a_backgrounded_child_even_after_the_run_has_finished() {
        let f = Fixture::new("stop-finished");
        let child_pid_path = f.root.join("child.pid");
        f.start(
            "build-demo",
            &format!("sleep 60 & echo $! >{}", child_pid_path.display()),
        );
        assert_eq!(f.settle("build-demo"), RunState::Exited(0));

        let child_pid: u32 = std::fs::read_to_string(&child_pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            crate::headless::alive(child_pid),
            "the background child is still running"
        );

        f.runs.stop("build-demo");
        assert!(
            !crate::headless::alive(child_pid),
            "stop must reach a backgrounded child even though the run's own \
             wrapper had already exited"
        );
    }

    /// A run whose process is gone without an exit code — killed, or the machine
    /// went down — is over, and there is no code to route on. Reporting it as
    /// still running would park the task there forever.
    ///
    /// It is `Interrupted` rather than `Exited(1)`, and the difference is the
    /// whole point: a failure routes down `on_fail` and spends an agent turn,
    /// where an interruption routes nowhere and runs the command again. This
    /// used to read as a failure, and a dispatcher stopping at its output
    /// ceiling therefore looked exactly like every suite it killed going red.
    #[test]
    fn a_run_killed_from_outside_reads_as_interrupted() {
        let f = Fixture::new("killed");
        let pid = f.start("bench-demo", "sleep 60");
        crate::headless::kill_group(pid);

        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(10) {
            if f.runs.state("bench-demo") != RunState::Running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(f.runs.state("bench-demo"), RunState::Interrupted);
    }

    /// The read of the exit file and the read of the wrapper's liveness are
    /// two separate moments, and a run that finished between them used to read
    /// as `Interrupted` — which the dispatcher answers by forgetting the run
    /// and starting the command over, so a command step that had passed was
    /// silently re-run and its log rolled aside. Rare, and only under load:
    /// `a_kept_log_is_not_mistaken_for_a_run` hit it once, reading
    /// `Interrupted` off `exit 0`.
    ///
    /// Asserted against the second read itself rather than through `state`,
    /// because the window it closes is a scheduling accident with no portable
    /// way to arrange on demand. What it pins is the reasoning: once the
    /// wrapper is gone the exit file no longer changes, so the answer comes
    /// from reading it again rather than from whichever moment the first read
    /// happened to catch.
    #[test]
    fn a_code_that_landed_before_the_wrapper_died_is_not_an_interruption() {
        let f = Fixture::new("late-exit-read");
        f.start("demo · gate", "exit 0");
        assert_eq!(f.settle("demo · gate"), RunState::Exited(0));

        assert_eq!(
            f.runs.what_a_dead_wrapper_left("demo · gate"),
            RunState::Exited(0),
            "a wrapper writes its code before it dies, so a dead one that \
             wrote a code has a code"
        );

        // With nothing there, the reading it always had: killed, or the
        // machine went down under it, and no code to route on.
        std::fs::remove_file(f.runs.exit_path("demo · gate")).unwrap();
        assert_eq!(
            f.runs.what_a_dead_wrapper_left("demo · gate"),
            RunState::Interrupted
        );
    }

    /// The pid `script_for_pane` records has to belong to the run itself, not
    /// to the pane's own shell — a pane's shell outlives every command run in
    /// it, the same way a terminal does, so a real pane feeds many scripts to
    /// one long-lived shell process over its life rather than starting a
    /// fresh one per run the way [`Fixture::start_in_pane`] does for every
    /// other test here. Simulated with a shell whose stdin this test keeps
    /// open past the run's own end, exactly what a live pane's shell does.
    ///
    /// `{ ... } 2>&1 | tee` puts the run's own group on the left of a pipe, a
    /// subshell whose `$$` is inherited from the invoking shell rather than
    /// its own — so the pid the wrapper's first line writes today names that
    /// invoking shell, which is still going long after this command is over.
    #[test]
    fn a_paned_runs_pid_ends_when_the_command_does() {
        let f = Fixture::new("paned-pid-lifetime");
        let key = "build-demo";
        let script = f
            .runs
            .script_for_pane(key, "true", &BTreeMap::new())
            .unwrap();

        let mut pane = std::process::Command::new("sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .current_dir(&f.root)
            .spawn()
            .unwrap();
        let mut pane_stdin = pane.stdin.take().unwrap();
        {
            use std::io::Write;
            pane_stdin.write_all(script.as_bytes()).unwrap();
            pane_stdin.flush().unwrap();
        }
        // Left open on purpose: closing it would end the pane's shell on its
        // own and prove nothing about the pid this run recorded.

        assert_eq!(f.settle(key), RunState::Exited(0));

        let pid = f
            .runs
            .read_pid(key)
            .expect("the pid file the wrapper wrote is still there to read");
        assert!(
            !crate::headless::alive(pid),
            "the pid recorded for `{key}` is still alive after the command it \
             names is over — it names the pane's own shell, not the run"
        );

        drop(pane_stdin);
        let _ = pane.wait();
    }

    /// The environment is exported before the `run:` line itself, in the
    /// same one-line form a lane's own environment uses — see
    /// `platform::env_export` — so the command inherits it.
    #[test]
    fn the_wrapper_exports_the_environment_before_the_run() {
        let f = Fixture::new("wrapper-body-env");
        let env = BTreeMap::from([("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string())]);
        let body = f.runs.wrapper_body("build-demo", "true", &env);
        assert!(
            body.contains("export SPOOLWAY_TASK='add-endpoint'\ntrue\n"),
            "{body}"
        );
    }
}
