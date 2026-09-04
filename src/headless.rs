//! Running lanes with no multiplexer: one detached process per turn.
//!
//! Everything the dispatcher decides is unchanged here. It still reconciles a
//! pass from the task file's `stage:` and the live lane list, and it still
//! reaches both through [`crate::mux::Mux`]. What this backend replaces is only
//! *where a lane lives*.
//!
//! ## A turn is a process
//!
//! Under a multiplexer an agent session is resident: it sits in its pane
//! between turns, and a prompt is text typed at something already running.
//! There is nothing to type at here, so a turn is spawned, runs, and exits —
//! which turns out to be the shape the pipeline already has. A step's lane
//! takes one turn and reports; the next step gets a lane of its own.
//!
//! That makes the status question, which is the hard part of any other backend,
//! nearly free: a process that is running is `Working` and one that has exited
//! is `Done`. There is no `Blocked`, because a `--print` run cannot stop and ask
//! — it simply ends, which the dispatcher already reads as "settled with an
//! unchanged stage" and routes to a person.
//!
//! ## What carries a conversation across turns
//!
//! The session id the profile already pins with `{session_id}`. Every profile
//! is validated to pass it (see [`crate::config`]) so that `spoolway eval --by` can
//! find a transcript afterwards; the same id is what lets a *second* turn
//! reopen the first one's conversation. How a kind spells that is one row of
//! [`crate::agent::ADAPTERS`] — this module knows that lanes resume, and
//! nothing about how any particular binary does it.
//!
//! ## What it costs
//!
//! There is no pane to attach to. Watching a lane means reading its log, and
//! answering one means resuming its session rather than typing at it. Both are
//! offered; live over-the-shoulder observation is not, and is the honest price
//! of the backend.
//!
//! The same price falls on reading a hung turn. A `--print` turn typically
//! buffers its answer until it ends, so the log is silent for exactly as long
//! as the model is working — indistinguishable from a hang. Nothing watches a
//! busy lane any more, on this backend or any other: a turn that never exits
//! just sits there, process and pane both, until a person happens to look.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::DispatchConfig;
use crate::mux::{Lane, LaneSpec, LaneStatus, Mux, Workspace, cut_worktree, worktree_root};
use crate::platform::Shell;

/// The dialect a turn's script is written in.
///
/// `Shell::Posix` rather than `Shell::CURRENT`, and not an oversight: this
/// backend reads `/proc` for liveness and needs `setsid` to detach a turn, so it
/// is a Linux backend today whatever the shell could be told to do. Naming the
/// dialect it actually emits keeps that honest, rather than implying a Windows
/// path that `alive` would not survive.
const SH: Shell = Shell::Posix;

/// Where lane records and logs live, under the project's home directory —
/// see [`crate::repo::Repo::headless_dir`].
pub(crate) const LANE_DIR: &str = "headless";

/// Marks a workspace that owns the checkout under it, and may therefore have it
/// removed. The other kind borrows somebody else's checkout — a plan closeout
/// running in place — and removing that would take a person's own worktree.
const OWNED: &str = "worktree";
const BORROWED: &str = "checkout";

/// The no-multiplexer backend.
#[derive(Debug, Clone)]
pub struct Headless {
    /// The project root: where the tracked control plane lives.
    root: PathBuf,
    /// Where task worktrees are cut.
    worktree_root: PathBuf,
    /// Where lane records and logs live — see
    /// [`crate::repo::Repo::headless_dir`]. Handed in at construction rather
    /// than resolved from `$HOME` on every call, the same way `worktree_root`
    /// is, so a test fixture points it at a scratch directory and nothing
    /// under test ever touches the real one.
    lanes_dir: PathBuf,
}

/// One lane, as it survives between dispatch passes.
///
/// On disk because a pass keeps nothing in memory between one and the next: a
/// dispatcher restarted mid-flight, or one that crashed and was started again,
/// has to see the lanes the last one left running. Everything the
/// dispatcher reads back out of [`Lane`] is here, plus what a *second* turn
/// needs — the args the first one ran with, so resuming is a rewrite of them
/// rather than a re-render nothing has kept the inputs for.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    name: String,
    kind: String,
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    cwd: PathBuf,
    /// The rendered opening args, including the `--session-id` that pins the
    /// conversation every later turn reopens.
    args: Vec<String>,
    env: BTreeMap<String, String>,
    path_prefix: Option<PathBuf>,
    /// The role this lane plays. A multiplexer puts it on the pane; with no
    /// pane to put it on it goes at the head of each turn in the log, which is
    /// the same job — telling whoever is reading which of six agents wrote this.
    #[serde(default)]
    label: String,
    /// Turns spawned so far. Zero means started but never prompted, which is
    /// [`LaneStatus::Idle`] — but only ever as the answer left when no turn has
    /// written a pid or an exit code. This is written after the turn it counts,
    /// so it lags a crash and never decides on its own whether one is running:
    /// see [`Headless::status`].
    turns: u32,
}

impl Headless {
    /// `lane_dir` is where lane records and logs live —
    /// [`crate::repo::Repo::headless_dir`] for every real caller.
    pub fn new(root: &Path, config: &DispatchConfig, lane_dir: PathBuf) -> Headless {
        Headless {
            root: root.to_path_buf(),
            worktree_root: worktree_root(root, config),
            lanes_dir: lane_dir,
        }
    }

    fn lane_dir(&self) -> PathBuf {
        self.lanes_dir.clone()
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.lane_dir().join(format!("{name}.json"))
    }

    /// A lane's whole output, across every turn it has taken.
    ///
    /// Appended to rather than replaced, and deliberately outliving the record:
    /// when a lane is torn down its transcript is the only account of what it
    /// did, and a person looking into a task that went wrong is looking for
    /// exactly that.
    fn log_path(&self, name: &str) -> PathBuf {
        self.lane_dir().join(format!("{name}.log"))
    }

    /// The pid and exit files a turn's shell writes — see
    /// [`crate::runfiles::RunFiles`], shared with [`crate::command_step::Runs`]
    /// which tracks a run the same way under its own directory.
    fn run_files(&self) -> crate::runfiles::RunFiles {
        crate::runfiles::RunFiles::new(self.lane_dir())
    }

    /// Written by a turn's shell as its last act, so its presence means the turn
    /// is over however the process ended.
    ///
    /// This is also what makes the liveness check safe against pid reuse: a pid
    /// that has been recycled to something unrelated would read as alive, but a
    /// finished turn has already said so here.
    fn exit_path(&self, name: &str) -> PathBuf {
        self.run_files().exit_path(name)
    }

    fn pid_path(&self, name: &str) -> PathBuf {
        self.run_files().pid_path(name)
    }

    fn load(&self, name: &str) -> Result<Record> {
        let raw = std::fs::read_to_string(self.record_path(name))
            .with_context(|| format!("no headless lane `{name}`"))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("lane record for `{name}` is unreadable"))
    }

    fn save(&self, record: &Record) -> Result<()> {
        std::fs::create_dir_all(self.lane_dir())?;
        crate::task::write_atomic(
            &self.record_path(&record.name),
            &serde_json::to_string_pretty(record)?,
        )
    }

    fn records(&self) -> Vec<Record> {
        let Ok(entries) = std::fs::read_dir(self.lane_dir()) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter_map(|raw| serde_json::from_str::<Record>(&raw).ok())
            .collect()
    }

    /// What the multiplexer would have told us about this lane.
    ///
    /// The files on disk are asked before `turns` is, and that order is the
    /// whole of the answer. `prompt` spawns the turn and only then increments
    /// `turns` and saves, so a dispatcher that dies inside that window leaves a
    /// record saying `0` over a process that is really running. Reading `turns`
    /// first called that lane `Idle`, `Idle` is settled, and the next
    /// dispatcher nudged a lane that was never idle — opening a second turn
    /// beside the live one. Whether a turn is running is a question about the
    /// process, so the process is what decides it.
    fn status(&self, record: &Record) -> LaneStatus {
        // Asked in this order on purpose. The exit file is written last by the
        // turn itself, so it is authoritative; the pid is only consulted for a
        // turn that has not said it finished.
        if self.exit_path(&record.name).exists() {
            return LaneStatus::Done;
        }
        if self.read_pid(&record.name).is_some_and(alive) {
            return LaneStatus::Working;
        }
        // Recorded but never spawned: no turn has written a pid and none has
        // left an exit code, which is precisely what `Idle` is documented to
        // mean. The dispatcher starts a lane and prompts it in the same breath,
        // so this is only ever seen between those two calls.
        if record.turns == 0 {
            return LaneStatus::Idle;
        }
        // Gone without writing an exit code: killed, or the machine went down
        // under it. Either way the turn is not running.
        LaneStatus::Done
    }

    fn read_pid(&self, name: &str) -> Option<u32> {
        self.run_files().read_pid(name)
    }

    /// Spawn one turn: the agent, its args, and the prompt it is to answer.
    ///
    /// Everything runs under one `sh`, which is what makes a turn observable
    /// from another process: the shell records its own pid before starting the
    /// agent and the agent's exit code after it, so "is this lane working" is a
    /// question about files rather than about a child this process would have to
    /// still be alive to have.
    fn spawn(&self, record: &Record, args: &[String], prompt: &str) -> Result<u32> {
        std::fs::create_dir_all(self.lane_dir())?;

        // A stale exit code would make the turn about to start look finished
        // before it has written a line.
        self.run_files().clear(&record.name);

        let mut script = String::new();

        // stdin from /dev/null, not left attached: an agent that finds no stdin
        // and was not told so waits on it — claude stalls three seconds and says
        // as much — and a pipeline pays that on every turn it ever runs.
        script.push_str(&format!(
            "exec </dev/null >>{} 2>&1\n",
            SH.quote(&self.log_path(&record.name).display().to_string())
        ));
        script.push_str(&format!(
            "echo $$ >{}\n",
            SH.quote(&self.pid_path(&record.name).display().to_string())
        ));

        // A PATH prefix goes on first, because the agent is resolved through
        // this PATH — the same mechanism a herdr lane gets, so one story covers
        // both backends rather than two. Nothing sets it in a real run; the
        // tests use it to put a stand-in agent in front of the real one.
        if let Some(prefix) = &record.path_prefix {
            script.push_str(&SH.path_export(prefix));
            script.push('\n');
        }
        // The same one-line export a herdr lane gets, from the same place — a
        // lane's environment is how `spoolway report` knows which task it
        // belongs to, and two spellings of it would be two ways to lose it.
        if !record.env.is_empty() {
            script.push_str(&SH.env_export(&record.env));
            script.push('\n');
        }

        // Both values go through `printf`'s arguments rather than into its
        // format string: a label is a prompt name today, and a `%` finding its
        // way into a format is a turn that logs nonsense or nothing.
        script.push_str(&format!(
            "printf '\\n=== turn %s (%s) ===\\n' {} {}\n",
            record.turns + 1,
            SH.quote(&record.label)
        ));

        // The kind's own name unless its row says the executable is called
        // something else. `start_lane` already refused a kind with no
        // headless row, so the adapter is always found here; the fallback to
        // `record.kind` is only for a row that somehow vanished from the
        // table between then and now.
        let program = crate::agent::adapter(&record.kind)
            .map(crate::agent::Adapter::program)
            .unwrap_or(&record.kind);
        let mut command = vec![SH.quote(program)];
        command.extend(args.iter().map(|arg| SH.quote(arg)));
        command.push(SH.quote(prompt));
        let command_line = command.join(" ");
        let exit_path = SH.quote(&self.exit_path(&record.name).display().to_string());

        script.push_str(&command_line);
        script.push('\n');
        // Last, so that its existence means the turn is genuinely over.
        script.push_str(&format!("echo $? >{exit_path}\n"));

        // `setsid`, so the turn outlives the dispatcher that started it. It also
        // makes the shell a process-group leader, which is what lets `stop_lane`
        // take the agent down with it instead of orphaning it. What we address
        // the turn by afterwards is the pid its own shell writes down.
        let spawned = std::process::Command::new("setsid")
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&record.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("could not spawn a lane: `setsid` is needed to detach it from this process")?;
        reap_when_it_ends(spawned);

        match await_pid_file(|| self.read_pid(&record.name)) {
            Some(pid) => Ok(pid),
            None => bail!(
                "lane `{}` was spawned but never reported a pid — see {}",
                record.name,
                self.log_path(&record.name).display()
            ),
        }
    }

    /// End a turn, and the agent under it.
    fn kill(&self, name: &str) {
        if let Some(pid) = self.read_pid(name) {
            kill_group(pid);
        }
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        crate::repo::run(&self.root, "git", args)
    }
}

/// Collect a detached child once its turn is over, without waiting here.
///
/// `setsid` puts the turn in its own session, but it only *forks* when its
/// caller is already a process-group leader — and a dispatcher is not one. So
/// what it does instead is exec the shell in place, and the turn is this
/// process's own direct child however detached its session is.
///
/// A child nobody ever waits on becomes a zombie when it exits: the processes
/// are gone, the pid is not. That cost nothing while a pass was its own
/// short-lived process, because the turn was reparented to init the moment the
/// pass ended and init reaps what it inherits. A dispatcher runs for days, and
/// would hold one pid per lane it had ever started — with `/proc/<pid>` still
/// there, which is exactly what a watchdog checking whether it killed something
/// reads to decide it has not.
///
/// So a thread does the waiting: it blocks for the length of the turn and then
/// collects it. One thread per lane that is running, and none per lane that has
/// finished.
pub(crate) fn reap_when_it_ends(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Wait briefly for a detached process to write its pid file, `None` if it
/// never does.
///
/// The other half of the `setsid` handshake, shared with
/// [`crate::command_step`] like [`reap_when_it_ends`] is: the shell writes its
/// own pid because `setsid` may fork, so the pid spawned here is not reliably
/// the group that will later need signalling. Five seconds is far past a
/// shell's startup and far short of a person noticing; the caller says what
/// never arriving means.
pub(crate) fn await_pid_file(mut read: impl FnMut() -> Option<u32>) -> Option<u32> {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(5) {
        if let Some(pid) = read() {
            return Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

/// End the whole process group `pid` leads, insisting only if asking fails.
///
/// Shared with [`crate::command_step`], which detaches its runs the same way and
/// needs them to go down the same way: one implementation, so a background
/// command left running at cleanup is reached exactly as an agent is.
///
/// The leader exiting does not empty its group: a turn that backgrounded a
/// server and ended normally leaves the server running in a leaderless group,
/// which is exactly what the leader's own liveness cannot see. So membership is
/// what decides whether there is anything left to signal — and it is also the
/// pid-reuse guard, since a recycled pid is only a group again if something new
/// leads one under the same number.
///
/// It returns when the group is gone, not when the signal has been sent. Every
/// caller's next line assumes the processes are down — `cleanup` removes the
/// worktree one was writing into, and the dispatcher routes the task onwards —
/// so a function that only *asks* would be a promise none of them could keep.
#[cfg(unix)]
pub fn kill_group(pid: u32) {
    // Group 0 is "everything in *our* group": the dispatcher, and every lane
    // under it. No lane's group is ever numbered that low, so a pid this small
    // is a bug somewhere upstream, and the blast radius of humouring it is the
    // whole run.
    if pid <= 1 || !group_alive(pid) {
        return;
    }
    signal_group(pid, libc::SIGTERM);
    // Give it a moment to go on its own before insisting.
    if settles_within(pid, std::time::Duration::from_secs(1)) {
        return;
    }
    signal_group(pid, libc::SIGKILL);
    // Not instant either: SIGKILL is delivered, and then the kernel tears the
    // processes down. Waiting out that gap is the difference between `stop`
    // and a suggestion.
    settles_within(pid, std::time::Duration::from_secs(5));
}

/// End the job [`spawn_detached`] put `pid` in, and everything still running
/// under it.
///
/// [`Headless::is_available`] still refuses this backend on Windows —
/// `setsid` is what detaches a *lane's* turn and there is no such thing — so
/// in practice this is reached only through [`crate::command_step`], which
/// has its own Windows spawn. Kept here rather than there because it is the
/// same shared implementation [`alive`] already is: one place that answers
/// "is it still going" and "end it", for whichever caller starts a pid this
/// way.
///
/// The job is the first choice and `taskkill /T /F` the fallback, because
/// each covers what the other cannot. A job's name is only findable while
/// somebody still holds a handle to it — see [`spawn_detached`], which keeps
/// its handle open for exactly this lookup — so a run spawned by a process
/// that has since exited, or by the pane backend, which never made a job at
/// all (`Runs::script_for_pane`, when a step carries no `headless:` key),
/// opens nothing here. `taskkill /T` walks the live parent-child tree
/// instead: no handle needed, but a grandchild whose parent already exited
/// is unlinked from that tree and escapes it, which is the hole the job
/// exists to close. Trying the job first and falling back is both answers,
/// each where it works.
#[cfg(windows)]
pub fn kill_group(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{OpenJobObjectW, TerminateJobObject};

    if pid == 0 {
        return;
    }
    let name = job_name(pid);
    // SAFETY: `OpenJobObjectW` with a name, checked for null before use;
    // `TerminateJobObject` on the handle it returns; `CloseHandle` once, on
    // the one path that opened it.
    let job_found = unsafe {
        let job = OpenJobObjectW(JOB_OBJECT_TERMINATE, 0, name.as_ptr());
        if !job.is_null() {
            TerminateJobObject(job, 1);
            CloseHandle(job);
        }
        !job.is_null()
    };
    if !job_found {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // `TerminateJobObject` only asks; every caller's next line assumes the
    // process is actually down — see this function's Unix twin, which waits
    // out the same gap after its own signal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline && crate::lock::is_running(pid) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// No portable notion of a process group on anything else this builds for.
#[cfg(not(any(unix, windows)))]
pub fn kill_group(_pid: u32) {}

/// Job-object access needed to reopen a run's job by name and end it.
/// windows-sys does not export this bit — it is written out the way
/// `lock.rs`'s `SYNCHRONIZE` is: a fixed part of the Win32 ABI, unchanged
/// since the API shipped.
#[cfg(windows)]
const JOB_OBJECT_TERMINATE: u32 = 0x0008;

/// The name a run's job is opened and reopened by — its own pid, which
/// [`kill_group`] is always given the same way `group_alive` is on Unix:
/// read back off the pid file [`spawn_detached`]'s wrapper wrote as its first
/// act. `Local\` keeps it out of the global namespace, which only a service
/// session would need to reach into on purpose.
#[cfg(windows)]
fn job_name(pid: u32) -> Vec<u16> {
    format!("Local\\spoolway-job-{pid}\0")
        .encode_utf16()
        .collect()
}

/// Spawn `command` detached: a new process group, so it does not answer to
/// this console's own signals, and a named job object, so [`kill_group`] can
/// find and end the whole tree it grows later — from a different process,
/// by pid alone, holding no handle of its own.
///
/// This is Windows' side of what `setsid` gives the Unix path: a run that
/// outlives the pass that started it. It is not `CREATE_SUSPENDED` plus a
/// job assignment before anything in the child runs — `std::process::Command`
/// has no way to reach the primary thread to resume it — so there is a gap
/// between the process starting and this call assigning it to the job. The
/// gap is not the shape of anything a wrapper script does as its first act,
/// which is why it is left rather than hand-rolling `CreateProcessW`.
///
/// The job handle is deliberately kept open — leaked — rather than closed
/// once the process is assigned. The object itself would survive on its
/// member processes alone, but its *name* would not: a named kernel object
/// drops out of the namespace when its last handle closes, and the name is
/// the only way [`kill_group`] ever reaches this job again. Closing the
/// handle here is what made every `stop` on a detached Windows run a silent
/// no-op. One leaked handle per spawned run, held for this process's
/// lifetime, is the price of the lookup; a run outliving this process falls
/// to `kill_group`'s `taskkill` fallback instead.
#[cfg(windows)]
pub(crate) fn spawn_detached(
    mut command: std::process::Command,
) -> std::io::Result<std::process::Child> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    let child = command.spawn()?;

    // SAFETY: `CreateJobObjectW` with a name and no security attributes,
    // checked for null; `AssignProcessToJobObject` on that job and the
    // process handle `spawn` just returned, which this process owns
    // outright. Best-effort: a job that failed to create or assign leaves
    // the process running undetached rather than not running at all, and
    // `kill_group` falls back to `taskkill` for a job it cannot open. The
    // job handle is never closed — see the doc comment above for why the
    // leak is the point.
    unsafe {
        let name = job_name(child.id());
        let job = CreateJobObjectW(std::ptr::null(), name.as_ptr());
        if !job.is_null() {
            AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
        }
    }

    Ok(child)
}

/// The negative pid is the whole process group: the shell is its leader thanks
/// to `setsid`, and signalling only the shell would leave the agent running
/// with nothing watching it.
///
/// A syscall rather than `kill(1)`, and the distinction is not academic. procps
/// takes `-TERM -1234` as two signal options rather than a signal and a group,
/// and refuses it — on the distro where that is the `kill` on PATH, every
/// `stop` in this file quietly did nothing and every lane outlived the
/// dispatcher. `lock.rs` has the same story about `kill -0`: a process-control
/// path that depends on which binary a machine happens to ship is a path that
/// is right until it is silently, invisibly wrong.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    // SAFETY: `kill` is async-signal-safe and takes no pointers; the worst a
    // wrong argument earns is ESRCH or EPERM, which is why the result is not
    // worth reading — the polling below is the real answer.
    unsafe { libc::kill(-(pid as i32), signal) };
}

/// Poll until nothing is left in the group, or the deadline passes. `true` if
/// it went.
///
/// The interval backs off geometrically from 20ms rather than staying fixed
/// there: `group_alive` below is a full scan of `/proc`, and the ordinary
/// case is a process that dies within the first poll or two of a signal —
/// paying for up to 300 of those scans at a flat 20ms, which the six-second
/// `SIGKILL` patience worked out to, bought nothing over checking less often
/// once the first few checks have already come back "still there".
#[cfg(unix)]
fn settles_within(pid: u32, patience: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + patience;
    let mut interval = std::time::Duration::from_millis(20);
    const MAX_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
    while std::time::Instant::now() < deadline {
        if !group_alive(pid) {
            return true;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(MAX_INTERVAL);
    }
    !group_alive(pid)
}

/// Is anything still running in this process group?
///
/// The same `/proc` [`alive`] reads, for the same reason: a zombie is a process
/// that has already stopped running and is only waiting to be reaped, and
/// counting one as a member would mean waiting out the full patience above
/// every time — the leader is this process's own child, so nothing reaps it
/// until the test or the dispatcher does.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    let group = pid.to_string();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            return false;
        };
        // `pid (comm) state ppid pgrp …`, and comm may itself contain spaces
        // and parentheses, so every field is counted from the *last* `)`.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let pgrp = fields.nth(1);
        state != Some("Z") && pgrp == Some(group.as_str())
    })
}

/// Is this pid a live process?
///
/// `/proc` rather than `kill -0`, because it also answers the question `kill -0`
/// cannot: a zombie is still signallable and is not still running. In practice
/// `setsid` means init reaps our turns and no zombie is ever seen, which is
/// exactly why this must not be the thing keeping that true.
#[cfg(target_os = "linux")]
pub fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `stat` is `pid (comm) state …`, and comm may itself contain spaces and
    // parentheses — so the state is the first field after the *last* `)`.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

/// No `/proc` here, so the answer comes from [`crate::lock::is_running`] —
/// Windows has no zombie state for it to misread (a pid whose process exited
/// reads as exited however many handles are still open on it, which that
/// function's zero-length wait already distinguishes), and the non-Linux unix
/// arm's `kill -0` is the portable best that platform offers. Reading `/proc`
/// unconditionally here made every pid on Windows read dead, so a command
/// step's run with a live wrapper reported `Interrupted` on the very pass
/// that started it.
#[cfg(not(target_os = "linux"))]
pub fn alive(pid: u32) -> bool {
    crate::lock::is_running(pid)
}

/// A branch name as one directory: `task/add-endpoint` is a path with a
/// component in it, and every task's worktree would otherwise nest under a
/// shared `task/` directory that nothing owns or cleans up.
fn branch_slug(branch: &str) -> String {
    branch.replace('/', "-")
}

/// The id a workspace is addressed by afterwards.
///
/// It carries the checkout path rather than pointing at a table holding one:
/// a workspace is a directory here, and an id that already says which one
/// cannot go stale against a registry, be lost when a record is cleaned up, or
/// mean something different in the next pass.
fn workspace_id(kind: &str, path: &Path) -> String {
    format!("headless:{kind}:{}", path.display())
}

/// The path back out of a workspace id, and whether it is ours to remove.
fn workspace_path(id: &str) -> Option<(&str, PathBuf)> {
    let rest = id.strip_prefix("headless:")?;
    let (kind, path) = rest.split_once(':')?;
    Some((kind, PathBuf::from(path)))
}

impl Mux for Headless {
    fn name(&self) -> &'static str {
        "headless"
    }

    fn is_available(&self) -> bool {
        // Nothing to connect to — the question is only whether a lane could be
        // written down and detached. `setsid` is the one thing here that is not
        // guaranteed to exist, and a pipeline that discovers it is missing one
        // lane at a time is a pipeline that discovers it too late.
        std::fs::create_dir_all(self.lane_dir()).is_ok()
            && std::process::Command::new("setsid")
                .arg("true")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
    }

    fn unavailable(&self) -> String {
        format!(
            "headless lanes cannot be started here: `{}` must be writable and `setsid` must be \
             on PATH (it detaches a turn so it outlives the dispatcher)",
            self.lane_dir().display()
        )
    }

    /// No. A turn that has ended is a process that has exited, so a lane waiting
    /// on a person is holding nothing — and a worker slot it kept would be a
    /// slot spent on an approval somebody has not got to yet.
    fn resident_while_waiting(&self) -> bool {
        false
    }

    fn list_lanes(&self) -> Result<Vec<Lane>> {
        // No ownership test to make: unlike a multiplexer, which is full of
        // sessions a person started by hand, this directory holds exactly the
        // lanes spoolway wrote into it and nothing else can appear in it.
        Ok(self
            .records()
            .into_iter()
            .map(|record| Lane {
                status: self.status(&record),
                name: record.name,
                kind: record.kind,
                pane_id: record.pane_id,
                tab_id: record.tab_id,
                workspace_id: record.workspace_id,
                cwd: record.cwd,
            })
            .collect())
    }

    fn create_workspace(
        &self,
        cwd: &Path,
        // Not where the checkout goes here: the root is configured, and the
        // directory under it is named after the branch, which is what
        // `worktree list` shows and so what a person looking for it reads.
        _task: &str,
        branch: &str,
        base: &str,
        _label: &str,
    ) -> Result<Workspace> {
        let path = self.worktree_root.join(branch_slug(branch));
        cut_worktree(cwd, &path, branch, base)?;

        Ok(Workspace {
            workspace_id: workspace_id(OWNED, &path),
            pane_id: new_pane_id(&path),
            tab_id: None,
            checkout_path: path,
        })
    }

    fn remove_workspace(&self, id: &str) -> Result<()> {
        let Some((kind, path)) = workspace_path(id) else {
            bail!("`{id}` is not a headless workspace id");
        };
        // The trait says never to call this for a borrowed checkout. Refusing
        // rather than trusting that, because the cost of being wrong is a
        // person's own worktree.
        if kind != OWNED {
            bail!(
                "workspace `{id}` borrows a checkout it does not own — removing it would take \
                 somebody else's worktree"
            );
        }
        self.git(&["worktree", "remove", "--force", &path.display().to_string()])?;
        Ok(())
    }

    /// Nothing to close: a workspace that owns no worktree is a directory
    /// somebody else's, and this backend put no window around it.
    fn close_workspace(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn close_tab(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn create_pane(&self, cwd: &Path, _label: &str) -> Result<Workspace> {
        Ok(Workspace {
            workspace_id: workspace_id(BORROWED, cwd),
            pane_id: new_pane_id(cwd),
            tab_id: None,
            checkout_path: cwd.to_path_buf(),
        })
    }

    /// A place for a lane to run, which here is a name and a directory that
    /// exists. The directory is checked because the dispatcher reads a failure
    /// here as "this task file came from another machine" and cuts a fresh
    /// worktree — which is the right answer for a checkout that is gone.
    fn split_pane(&self, _parent: &str, cwd: &Path) -> Result<String> {
        if !cwd.is_dir() {
            bail!(
                "{} is not a directory, so no lane can be run in it",
                cwd.display()
            );
        }
        Ok(new_pane_id(cwd))
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        // A pane with no lane in it is the ordinary case, not an error: the
        // dispatcher takes a pane back when a launch fails, and that pane never
        // held anything.
        for record in self.records() {
            if record.pane_id == pane_id {
                self.kill(&record.name);
                let _ = std::fs::remove_file(self.record_path(&record.name));
            }
        }
        Ok(())
    }

    /// Write the lane down. Nothing is spawned yet — a turn is a process, and
    /// the lane has not been given anything to answer.
    fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()> {
        // Refused here rather than at the first prompt: a kind nobody has
        // established headless flags for would otherwise be launched with a
        // guessed one, and a guess at `--resume` loses a lane's context without
        // saying so.
        let adapter = crate::agent::adapter(spec.kind);
        if adapter.and_then(|a| a.headless.as_ref()).is_none() {
            bail!(
                "`{}` cannot run headless: spoolway does not know how to hand it a prompt and \
                 reopen its session. Run this project under a multiplexer \
                 (`dispatch.backend = \"herdr\"` or `\"tmux\"`), or use one of the kinds that \
                 can: {}",
                spec.kind,
                headless_kinds()
            );
        }

        self.save(&Record {
            name: spec.name.to_string(),
            kind: spec.kind.to_string(),
            pane_id: spec.pane_id.to_string(),
            // Neither exists here, and both are only ever handed back to this
            // backend, which ignores them.
            workspace_id: String::new(),
            tab_id: String::new(),
            // The pane the dispatcher just cut says where this lane belongs.
            // It matters beyond running the process there: `lane.cwd` is what
            // the dispatcher checks to leave a branch alone while a lane works
            // in its checkout, and how a lane that might belong to another
            // project is told apart from ours.
            cwd: pane_cwd(spec.pane_id).unwrap_or_else(|| self.root.clone()),
            args: spec.args.to_vec(),
            env: spec.env.clone(),
            path_prefix: spec.path_prefix.map(Path::to_path_buf),
            label: spec.label.to_string(),
            turns: 0,
        })
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        let mut record = self.load(name)?;

        // A turn still running is not to be disturbed. Under a multiplexer this
        // is a race the pane arbitrates by putting the text in an input box;
        // here a second process would open the same session twice.
        if self.status(&record) == LaneStatus::Working {
            bail!("lane `{name}` is mid-turn — nothing may be sent to it until it ends");
        }

        let adapter = crate::agent::adapter(&record.kind)
            .filter(|adapter| adapter.headless.is_some())
            .with_context(|| format!("`{}` cannot run headless", record.kind))?;

        // The first turn opens the session the lane is pinned to; every later
        // one reopens it, which for some kinds is the same args, for others a
        // rewrite, and for others again a subcommand appended after the print
        // form. Which is which — and in what order — is the adapter's business.
        let args = adapter
            .headless_args(&record.args, record.turns > 0)
            .with_context(|| format!("`{}` cannot run headless", record.kind))?;

        self.spawn(&record, &args, text)?;
        record.turns += 1;
        self.save(&record)
    }

    fn read(&self, name: &str, lines: usize) -> Result<String> {
        let log = std::fs::read_to_string(self.log_path(name)).unwrap_or_default();
        let tail: Vec<&str> = log
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(tail.join("\n"))
    }

    /// No keyboard reaches into a running turn here, so ending it is the only
    /// interrupt this backend has — a headless lane keeps no session between
    /// turns for an interrupt to spare.
    ///
    /// The one thing this does that [`Mux::stop_lane`] does not: bank the
    /// interrupted turn first. The dispatcher banks a lane through
    /// `record_usage` before it tears one down, but `queue pause` and the
    /// board's `p`/`P` interrupt straight through the backend with no
    /// dispatcher in the call — so without this the killed turn's tokens are
    /// lost, the record gone before anything accounted for it (review finding
    /// 36). Best-effort: a project that will not open, or a lane whose session
    /// is not on record, is stopped anyway rather than kept alive.
    fn interrupt_lane(&self, name: &str) -> Result<()> {
        if let Ok(repo) = crate::repo::Repo::discover(&self.root)
            && let Some((kind, session)) = crate::dispatch::lane_session(&repo, name)
            && let Some((task, step)) = name.split_once(" · ")
        {
            crate::usage::bank_lane(&repo, &kind, &session, task, step);
        }
        self.stop_lane(name, "")
    }

    // [`Mux::vacate_lane`] is not implemented here either, for a simpler
    // reason than tmux's: there is no pane. A headless turn is a process that
    // runs to completion and exits, so there is nothing resident to ask to
    // leave and nothing left standing to hand back. The trait's default
    // defers to `stop_lane` below, which is exactly what a headless lane has
    // always done to end.

    fn stop_lane(&self, name: &str, _pane_id: &str) -> Result<()> {
        self.kill(name);
        // The record goes; the log stays. One is bookkeeping the next pass
        // would trip over, the other is the only account of what this lane did.
        let _ = std::fs::remove_file(self.record_path(name));
        self.run_files().clear(name);
        Ok(())
    }

    /// Nothing here to focus: a headless lane is a process and a log file, and
    /// the log is where a person reads what it did. The pane it does not have
    /// is kept just as faithfully.
    fn focus_lane(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
        Ok(())
    }

    fn rename_workspace(&self, _workspace_id: &str, _label: &str) -> Result<()> {
        Ok(())
    }

    fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
        Ok(())
    }
}

/// Panes are minted rather than allocated: this backend has none, and what the
/// dispatcher needs from one is only a handle it can hand back later.
///
/// It carries the directory the lane will run in for the same reason a
/// workspace id carries its checkout — the alternative is a second registry
/// that has to be kept in step with this one. A multiplexer binds a pane to a
/// cwd when it makes it and hands back an opaque id; here the id *is* the
/// binding, which is what lets `start_lane` know where to run without the
/// dispatcher having to pass it twice.
fn new_pane_id(cwd: &Path) -> String {
    format!(
        "headless:pane:{}:{}",
        crate::usage::new_session_id(),
        cwd.display()
    )
}

/// The directory a pane id was minted against.
fn pane_cwd(pane_id: &str) -> Option<PathBuf> {
    // `split_once` on the id's own separator, so a path containing a colon
    // survives — everything after the mint is the path.
    pane_id
        .strip_prefix("headless:pane:")?
        .split_once(':')
        .map(|(_, path)| PathBuf::from(path))
}

/// The kinds that can run without a multiplexer, for the refusal that names them.
fn headless_kinds() -> String {
    crate::agent::ADAPTERS
        .iter()
        .filter(|adapter| adapter.headless.is_some())
        .map(|adapter| adapter.kind)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::home;

    /// A backend rooted in a scratch directory, plus a `bin/` on the lane's
    /// PATH holding a stand-in for the agent.
    ///
    /// The stand-in matters more than it looks: `path_prefix` is resolved by
    /// the same launch path a real agent is, so running the tests through it
    /// exercises how a lane actually starts rather than a simulation of it.
    /// Everything below spawns real processes and
    /// reads their real output; nothing here talks to a model.
    ///
    /// Which is why the tests that take a turn are `#[cfg(unix)]`. A turn is
    /// `setsid sh -c`, its liveness is `/proc`, and the stand-in agent is a
    /// `#!/bin/sh` script — the module doc says outright that this is a Linux
    /// backend, and `is_available` refuses to start a lane anywhere `setsid`
    /// is missing. What is left running on Windows is everything that decides
    /// something without launching anything: the id round trips, the branch
    /// slug, where worktrees are cut, and the refusals.
    struct Fixture {
        root: PathBuf,
        #[cfg_attr(not(unix), allow(dead_code))]
        bin: PathBuf,
        mux: Headless,
    }

    impl Fixture {
        fn new(name: &str) -> Fixture {
            let root = crate::scratch::root(&format!("headless-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();

            let mut config = DispatchConfig::default();
            config.worktree_root = root.join("worktrees").display().to_string();

            Fixture {
                mux: Headless::new(&root, &config, root.join(LANE_DIR)),
                root,
                bin,
            }
        }

        /// Put a script on the lane's PATH under the name of a real agent kind,
        /// so the backend launches it exactly as it would the real thing.
        ///
        /// `#!/bin/sh` and `chmod +x`, which is the honest shape of the thing
        /// being tested — see the note on [`Fixture`] about why the tests that
        /// use this one are POSIX-only.
        #[cfg(unix)]
        fn agent(&self, kind: &str, body: &str) -> &Fixture {
            let path = self.bin.join(kind);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::process::Command::new("chmod")
                .args(["+x", &path.display().to_string()])
                .status()
                .unwrap();
            self
        }

        /// A lane placed in the fixture root, ready to be prompted.
        #[cfg(unix)]
        fn lane(&self, name: &str, kind: &str, args: &[&str]) -> String {
            let pane = new_pane_id(&self.root);
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            self.mux
                .start_lane(&LaneSpec {
                    name,
                    label: "implementer",
                    kind,
                    pane_id: &pane,
                    args: &args,
                    env: &BTreeMap::new(),
                    path_prefix: Some(&self.bin),
                })
                .unwrap();
            pane
        }

        #[cfg(unix)]
        fn status(&self, name: &str) -> LaneStatus {
            self.mux
                .list_lanes()
                .unwrap()
                .into_iter()
                .find(|lane| lane.name == name)
                .map(|lane| lane.status)
                .unwrap_or(LaneStatus::Unknown)
        }

        /// Block until the lane's turn is over, so an assertion about a finished
        /// turn is never a race with a process that is still starting.
        #[cfg(unix)]
        fn settle(&self, name: &str) {
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(20) {
                if self.status(name) == LaneStatus::Done {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            panic!(
                "lane `{name}` never finished its turn; log:\n{}",
                self.mux.read(name, 50).unwrap_or_default()
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The whole lifecycle of one lane, against a real process.
    #[cfg(unix)]
    #[test]
    fn a_lane_is_idle_until_prompted_then_working_then_done() {
        let f = Fixture::new("lifecycle");
        // Long enough to be observed mid-turn, short enough not to slow the run.
        f.agent("pi", "sleep 1; echo finished");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);

        assert_eq!(
            f.status("implement-demo"),
            LaneStatus::Idle,
            "a lane written down but never prompted has not started a turn"
        );

        f.mux.prompt("implement-demo", "go").unwrap();
        assert_eq!(
            f.status("implement-demo"),
            LaneStatus::Working,
            "the turn is running"
        );

        f.settle("implement-demo");
        assert!(
            f.mux
                .read("implement-demo", 20)
                .unwrap()
                .contains("finished"),
            "the lane's output is its log"
        );
    }

    /// `Done` is the resting state a settled lane sits in, and the dispatcher
    /// reads it as "ended its turn, may be prompted again". A backend that
    /// reported an exited turn as anything else would strand every lane.
    #[cfg(unix)]
    #[test]
    fn a_finished_turn_settles_rather_than_disappearing() {
        let f = Fixture::new("settled");
        f.agent("pi", "echo done");
        f.lane("review-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("review-demo", "go").unwrap();
        f.settle("review-demo");

        let lanes = f.mux.list_lanes().unwrap();
        let lane = lanes.iter().find(|l| l.name == "review-demo").unwrap();
        assert!(lane.status.is_settled());
        assert!(!lane.status.is_busy());
    }

    /// The claim the whole backend rests on: a second turn reopens the first
    /// one's conversation. The stand-in records the argv it was handed, so this
    /// asserts what the agent would actually have received.
    #[cfg(unix)]
    #[test]
    fn a_second_turn_resumes_the_session_the_first_one_opened() {
        let f = Fixture::new("resume");
        f.agent("claude", r#"echo "ARGV: $*""#);
        f.lane(
            "implement-demo",
            "claude",
            &["--model", "m", "--session-id", "u1"],
        );

        f.mux.prompt("implement-demo", "first").unwrap();
        f.settle("implement-demo");
        f.mux.prompt("implement-demo", "second").unwrap();
        f.settle("implement-demo");

        let log = f.mux.read("implement-demo", 100).unwrap();
        assert!(
            log.contains("ARGV: --model m --session-id u1 --print first"),
            "the opening turn creates the session:\n{log}"
        );
        assert!(
            log.contains("ARGV: --model m --resume u1 --print second"),
            "the second turn must reopen it, not start a fresh one:\n{log}"
        );
    }

    /// pi's flag continues a session it already has, so its second turn is the
    /// same argv — the other half of the adapter contract.
    #[cfg(unix)]
    #[test]
    fn a_pi_lane_keeps_its_session_flag_across_turns() {
        let f = Fixture::new("resume-pi");
        f.agent("pi", r#"echo "ARGV: $*""#);
        f.lane("implement-demo", "pi", &["--session-id", "u1"]);

        f.mux.prompt("implement-demo", "first").unwrap();
        f.settle("implement-demo");
        f.mux.prompt("implement-demo", "second").unwrap();
        f.settle("implement-demo");

        let log = f.mux.read("implement-demo", 100).unwrap();
        assert!(log.contains("ARGV: --session-id u1 --print first"), "{log}");
        assert!(
            log.contains("ARGV: --session-id u1 --print second"),
            "{log}"
        );
    }

    /// One log per lane, across every turn — which is what the reminder loop
    /// hashes to decide whether a lane is making progress.
    #[cfg(unix)]
    #[test]
    fn a_lanes_log_accumulates_across_its_turns() {
        let f = Fixture::new("log-accumulates");
        f.agent("pi", r#"echo "said $*""#);
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);

        for turn in ["one", "two", "three"] {
            f.mux.prompt("implement-demo", turn).unwrap();
            f.settle("implement-demo");
        }

        let log = f.mux.read("implement-demo", 200).unwrap();
        for turn in ["one", "two", "three"] {
            assert!(log.contains(turn), "turn `{turn}` is missing from:\n{log}");
        }
        // Each turn is headed by its number and the lane's role. With no pane to
        // carry a label, this is the only thing telling a reader which of six
        // agents wrote what follows.
        assert!(log.contains("=== turn 3 (implementer) ==="), "{log}");
    }

    /// `read` is the reminder loop's fallback progress signal, so it has to be
    /// bounded the way a pane's scrollback is.
    #[cfg(unix)]
    #[test]
    fn reading_a_lane_returns_only_the_tail() {
        let f = Fixture::new("tail");
        f.agent("pi", "seq 1 200");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "go").unwrap();
        f.settle("implement-demo");

        let tail = f.mux.read("implement-demo", 10).unwrap();
        assert_eq!(tail.lines().count(), 10);
        assert!(tail.contains("200"), "the tail is the newest end: {tail}");
        assert!(!tail.contains("\n1\n"), "not the oldest: {tail}");
    }

    /// A turn still running is not to be disturbed: a second process would open
    /// the same session twice and one of them would lose.
    #[cfg(unix)]
    #[test]
    fn a_lane_mid_turn_refuses_a_second_prompt() {
        let f = Fixture::new("busy");
        f.agent("pi", "sleep 3");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "first").unwrap();

        let refused = f.mux.prompt("implement-demo", "second");
        assert!(refused.is_err(), "a busy lane accepted a prompt");
        assert!(refused.unwrap_err().to_string().contains("mid-turn"));

        f.mux.stop_lane("implement-demo", "").unwrap();
    }

    /// A dispatcher killed between `prompt`'s spawn and its save leaves a
    /// record saying no turn has run over a turn that is running right now.
    /// The next dispatcher has only these files to read, and reading `turns`
    /// ahead of them made it call that lane `Idle` — which is settled, so it
    /// nudged a live lane and opened a second turn beside the first.
    ///
    /// Written by hand rather than by killing a dispatcher, because the window
    /// is a few microseconds wide: the e2e `disaster` suite hits it by chance
    /// about one run in four, which is a flake rather than a test.
    #[cfg(unix)]
    #[test]
    fn a_running_turn_outranks_a_record_that_never_recorded_it() {
        let f = Fixture::new("crash-window");
        f.agent("pi", "sleep 60");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "go").unwrap();
        assert_eq!(f.status("implement-demo"), LaneStatus::Working);

        // Exactly what the crash leaves behind: the turn still running, and the
        // record rolled back to the state `start_lane` wrote.
        let mut record = f.mux.load("implement-demo").unwrap();
        let pid = f.mux.read_pid("implement-demo").unwrap();
        record.turns = 0;
        f.mux.save(&record).unwrap();

        assert!(
            alive(pid),
            "the turn must still be running for this to test"
        );
        assert_eq!(
            f.status("implement-demo"),
            LaneStatus::Working,
            "a live turn read as settled, so the next pass would prompt over it"
        );
        assert!(
            f.mux.prompt("implement-demo", "again").is_err(),
            "a second turn was allowed to open beside the live one"
        );

        f.mux.stop_lane("implement-demo", "").unwrap();
    }

    /// Tearing a lane down has to take the agent with it, not just the shell
    /// that started it — the shell is a process-group leader precisely so this
    /// can reach through it.
    #[cfg(unix)]
    #[test]
    fn stopping_a_lane_kills_the_turn_under_it() {
        let f = Fixture::new("stop");
        f.agent("pi", "sleep 60");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "go").unwrap();
        assert_eq!(f.status("implement-demo"), LaneStatus::Working);

        let pid = f.mux.read_pid("implement-demo").unwrap();
        f.mux.stop_lane("implement-demo", "").unwrap();

        assert!(!alive(pid), "the turn survived its lane being stopped");
        assert!(
            f.mux.list_lanes().unwrap().is_empty(),
            "a stopped lane must stop being listed, or every later pass sees a ghost"
        );
        assert!(
            f.mux.log_path("implement-demo").exists(),
            "the log is the only account of what the lane did and must outlive it"
        );
    }

    /// No keyboard reaches a running headless turn, so `interrupt_lane` ends
    /// it exactly as `stop_lane` would — see the doc comment on the impl.
    #[cfg(unix)]
    #[test]
    fn interrupting_a_lane_kills_the_turn_the_same_as_stopping_it() {
        let f = Fixture::new("interrupt");
        f.agent("pi", "sleep 60");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "go").unwrap();
        assert_eq!(f.status("implement-demo"), LaneStatus::Working);

        let pid = f.mux.read_pid("implement-demo").unwrap();
        f.mux.interrupt_lane("implement-demo").unwrap();

        assert!(!alive(pid), "the turn survived its lane being interrupted");
        assert!(
            f.mux.list_lanes().unwrap().is_empty(),
            "an interrupted headless lane must stop being listed, the same as a stopped one"
        );
    }

    /// `kill_group` sends `SIGTERM` first and only escalates to `SIGKILL`
    /// once the settle patience runs out — see its own doc comment. A leader
    /// that ignores `SIGTERM` is what actually exercises the escalation,
    /// rather than leaving it dead code every other test happens not to
    /// reach because a plain `sleep` already dies on the first signal.
    #[cfg(unix)]
    #[test]
    fn kill_group_escalates_to_sigkill_when_sigterm_is_ignored() {
        let dir = crate::scratch::root("kill-group-escalate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");

        let script = format!("echo $$ >{}; trap '' TERM; sleep 60", pidfile.display());
        let spawned = std::process::Command::new("setsid")
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        reap_when_it_ends(spawned);

        let pid = await_pid_file(|| std::fs::read_to_string(&pidfile).ok()?.trim().parse().ok())
            .expect("the shell must have written its own pid");
        assert!(alive(pid), "the process must be up before it can be killed");

        kill_group(pid);

        assert!(
            !alive(pid),
            "SIGKILL must reach a leader that ignores SIGTERM"
        );
    }

    /// A turn that backgrounds a server and ends normally leaves that server
    /// running in a leaderless group — see `kill_group`'s own doc comment for
    /// why group membership, not the leader's own liveness, is what it
    /// checks. A shell that backgrounds a child and exits immediately
    /// reproduces exactly that: the child inherits the leader's pgid under
    /// `setsid` and outlives it.
    #[cfg(unix)]
    #[test]
    fn kill_group_reaches_a_leaderless_group() {
        let dir = crate::scratch::root("kill-group-leaderless");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let leader_pidfile = dir.join("leader.pid");
        let orphan_pidfile = dir.join("orphan.pid");

        let script = format!(
            "echo $$ >{}; sleep 60 & echo $! >{}; exit 0",
            leader_pidfile.display(),
            orphan_pidfile.display()
        );
        let spawned = std::process::Command::new("setsid")
            .arg("sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        reap_when_it_ends(spawned);

        let leader = await_pid_file(|| {
            std::fs::read_to_string(&leader_pidfile)
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .expect("the shell must have written its own pid");
        let orphan = await_pid_file(|| {
            std::fs::read_to_string(&orphan_pidfile)
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .expect("the backgrounded sleep must have written its pid");
        assert!(
            alive(orphan),
            "the orphaned sleep must be running before the kill"
        );

        kill_group(leader);

        assert!(
            !alive(orphan),
            "kill_group must reach a group whose leader has already exited"
        );
    }

    /// The dispatcher takes a pane back when a launch fails, and that pane never
    /// held a lane. Refusing it would turn one failed start into a failed pass.
    #[test]
    fn closing_a_pane_that_never_held_a_lane_is_fine() {
        let f = Fixture::new("phantom");
        assert!(f.mux.close_pane(&new_pane_id(&f.root)).is_ok());
    }

    /// A kind nobody has established headless flags for is refused before
    /// anything is spawned. The alternative is guessing a resume flag at a real
    /// binary, whose failure mode is a lane answering with none of the context
    /// the question came from.
    #[test]
    fn a_kind_that_cannot_run_headless_is_refused_at_the_start() {
        let f = Fixture::new("unknown-kind");
        let pane = new_pane_id(&f.root);
        let refused = f.mux.start_lane(&LaneSpec {
            name: "implement-demo",
            label: "implementer",
            kind: "gemini",
            pane_id: &pane,
            args: &[],
            env: &BTreeMap::new(),
            path_prefix: None,
        });

        let message = refused.unwrap_err().to_string();
        assert!(message.contains("cannot run headless"), "{message}");
        // And it says what to do instead, both ways.
        assert!(message.contains("herdr"), "{message}");
        assert!(message.contains("pi"), "{message}");
    }

    /// The lane's environment is what `spoolway report` reads to know which task
    /// it belongs to, so a lane that loses it is a lane that cannot report.
    #[cfg(unix)]
    #[test]
    fn a_lanes_environment_reaches_its_turn() {
        let f = Fixture::new("env");
        f.agent("pi", r#"echo "TASK=$SPOOLWAY_TASK STEP=$SPOOLWAY_STEP""#);

        let env = BTreeMap::from([
            ("SPOOLWAY_TASK".to_string(), "add-endpoint".to_string()),
            ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
        ]);
        let pane = new_pane_id(&f.root);
        f.mux
            .start_lane(&LaneSpec {
                name: "implement-demo",
                label: "implementer",
                kind: "pi",
                pane_id: &pane,
                args: &["--session-id".to_string(), "s1".to_string()],
                env: &env,
                path_prefix: Some(&f.bin),
            })
            .unwrap();
        f.mux.prompt("implement-demo", "go").unwrap();
        f.settle("implement-demo");

        let log = f.mux.read("implement-demo", 20).unwrap();
        assert!(log.contains("TASK=add-endpoint STEP=implement"), "{log}");
    }

    /// A value with a quote in it has to arrive as one argument with nothing
    /// executed — the prompt is model-written text and reaches a shell.
    #[cfg(unix)]
    #[test]
    fn a_prompt_containing_shell_metacharacters_is_one_argument() {
        let f = Fixture::new("quoting");
        f.agent("pi", r#"printf 'GOT[%s]\n' "$4""#);
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);

        let hostile = "it's stuck'; touch /tmp/spoolway-pwned; :'";
        f.mux.prompt("implement-demo", hostile).unwrap();
        f.settle("implement-demo");

        let log = f.mux.read("implement-demo", 20).unwrap();
        assert!(log.contains(&format!("GOT[{hostile}]")), "{log}");
        assert!(
            !Path::new("/tmp/spoolway-pwned").exists(),
            "the embedded command ran"
        );
    }

    /// Removing a workspace removes a worktree, and the borrowed kind is
    /// somebody's own checkout. Refused rather than trusted, because the cost of
    /// being wrong is a person's work.
    #[test]
    fn a_borrowed_checkout_is_never_removed() {
        let f = Fixture::new("borrowed");
        std::fs::create_dir_all(&f.root).unwrap();
        let borrowed = f.mux.create_pane(&f.root, "closeout").unwrap();

        let refused = f.mux.remove_workspace(&borrowed.workspace_id);
        assert!(refused.is_err());
        assert!(refused.unwrap_err().to_string().contains("does not own"));
        assert!(f.root.is_dir(), "the checkout was removed anyway");
    }

    /// A pane id carries the directory its lane runs in, and `lane.cwd` is what
    /// the dispatcher checks before it moves a branch under a working lane —
    /// so a path that does not survive the round trip is a real race.
    #[test]
    fn a_pane_id_round_trips_its_directory() {
        let plain = Path::new("/home/x/work/task-a");
        assert_eq!(pane_cwd(&new_pane_id(plain)).as_deref(), Some(plain));

        // A colon is legal in a path and is also this id's own separator.
        let awkward = Path::new("/home/x/od:d/task-b");
        assert_eq!(pane_cwd(&new_pane_id(awkward)).as_deref(), Some(awkward));
    }

    /// A branch is a path with components in it, and every task's worktree would
    /// otherwise nest under a shared directory nothing owns.
    #[test]
    fn a_branch_becomes_one_directory() {
        assert_eq!(branch_slug("task/add-endpoint"), "task-add-endpoint");
        assert_eq!(branch_slug("plan/a/b"), "plan-a-b");
    }

    /// Worktrees must land outside the checkout: inside `.spoolway/` every
    /// lane's checkout would sit under the one path this project blocks writes
    /// to, so a lane editing its own files would block its own task.
    #[test]
    fn worktrees_are_cut_outside_the_project() {
        let root = Path::new("/home/x/dev/myproject");
        let default = worktree_root(root, &DispatchConfig::default());
        assert!(
            !default.starts_with(root),
            "worktrees landed inside the checkout: {}",
            default.display()
        );
        assert!(
            default.ends_with("myproject/worktrees"),
            "{}",
            default.display()
        );

        let mut config = DispatchConfig::default();
        config.worktree_root = "~/elsewhere".into();
        assert_eq!(worktree_root(root, &config), home().join("elsewhere"));
    }

    /// A workspace id carries its checkout so nothing has to keep a table of
    /// them, which is what makes it survive a record being cleaned up.
    #[test]
    fn a_workspace_id_round_trips_its_checkout() {
        let path = Path::new("/home/x/worktrees/task-a");
        let id = workspace_id(OWNED, path);
        assert_eq!(workspace_path(&id), Some((OWNED, path.to_path_buf())));
        assert_eq!(workspace_path("not-ours"), None);
    }

    /// This process must never be the one holding a turn alive: a dispatcher
    /// that exits mid-pass, or is restarted, must leave its lanes running.
    #[cfg(unix)]
    #[test]
    fn a_turn_outlives_the_process_that_started_it() {
        let f = Fixture::new("detached");
        f.agent("pi", "sleep 2; echo survived");
        f.lane("implement-demo", "pi", &["--session-id", "s1"]);
        f.mux.prompt("implement-demo", "go").unwrap();

        let pid = f.mux.read_pid("implement-demo").unwrap();
        // Its own session, not ours: that is what `setsid` bought, and it is why
        // a turn survives the dispatcher and is reaped by init rather than left
        // a zombie in a loop that runs for days.
        assert_ne!(
            session_of(pid),
            session_of(std::process::id()),
            "the turn shares this process's session and would go down with it"
        );
        // And it leads that session, which is what lets `stop_lane` signal the
        // whole group and take the agent down with the shell.
        assert_eq!(
            session_of(pid),
            Some(pid),
            "the turn's shell is not its session leader, so killing the group \
             would not reach the agent under it"
        );

        f.settle("implement-demo");
        assert!(
            f.mux
                .read("implement-demo", 10)
                .unwrap()
                .contains("survived")
        );
    }

    // -----------------------------------------------------------------------
    // Live: real agents, real models. `cargo test -- --ignored --nocapture`.
    // -----------------------------------------------------------------------
    //
    // Everything above proves the backend does what it means to. These prove
    // the thing it means to do is the right thing — that a real binary really
    // does reopen the conversation when handed the args this backend builds.
    //
    // That claim cannot be tested against a stand-in, because it is a claim
    // about the agent and not about spoolway: the failure it guards is a kind
    // that quietly starts a *fresh* session on a re-passed id, which would
    // answer a lane's question with none of the context the question came from
    // and say nothing about having done so. Both rows in `agent::ADAPTERS` were
    // settled by running these.
    //
    // Ignored by default: they need the binaries, they cost tokens, and one of
    // them needs a local model server.

    /// Teach a lane a fact in one turn, ask for it back in the next.
    ///
    /// The fact is a nonce, so a model cannot pass by guessing, and it is not
    /// in the second prompt — only a session that carried over can answer.
    #[cfg(unix)]
    fn resume_carries_context(kind: &str, model: &str, extra: &[&str]) {
        let nonce = format!("ZX{}", std::process::id());
        let f = Fixture::new(&format!("live-{kind}"));
        let session = crate::usage::new_session_id();

        let mut args: Vec<String> = vec!["--model".into(), model.into()];
        args.extend(extra.iter().map(|a| a.to_string()));
        args.extend(["--session-id".to_string(), session]);

        let pane = new_pane_id(&f.root);
        f.mux
            .start_lane(&LaneSpec {
                name: "implement-live",
                label: "implementer",
                kind,
                pane_id: &pane,
                args: &args,
                env: &BTreeMap::new(),
                path_prefix: None,
            })
            .unwrap();

        f.mux
            .prompt(
                "implement-live",
                &format!("Remember this code: {nonce}. Reply with just the word OK."),
            )
            .unwrap();
        f.settle("implement-live");

        f.mux
            .prompt(
                "implement-live",
                "What was the code I asked you to remember? Reply with just the code.",
            )
            .unwrap();
        f.settle("implement-live");

        let log = f.mux.read("implement-live", 200).unwrap();
        // Split on the turn number alone, not the whole header: the header also
        // carries the lane's role, and a test that pins its exact wording fails
        // for a reason that has nothing to do with what it is checking.
        let second = log.split("=== turn 2 ").nth(1).unwrap_or("");
        assert!(
            second.contains(&nonce),
            "`{kind}` did not carry its session across turns — the second turn opened a fresh \
             conversation, which is the silent context loss the adapter row exists to prevent.\
             \n\nturn 2 was:\n{second}\n\nwhole log:\n{log}"
        );
    }

    /// claude resumes only because its args are rewritten: `--session-id` twice
    /// is an error, so this also proves the swap is being applied to a real
    /// binary and not merely to a vector of strings.
    #[cfg(unix)]
    #[test]
    #[ignore = "live: needs `claude` and spends tokens"]
    fn live_a_claude_lane_resumes_its_session() {
        resume_carries_context("claude", "claude-haiku-4-5-20251001", &[]);
    }

    /// pi resumes on the args it started with. Needs a local model server.
    #[cfg(unix)]
    #[test]
    #[ignore = "live: needs `pi` and a local model server"]
    fn live_a_pi_lane_resumes_its_session() {
        resume_carries_context("pi", "Qwen3.6-35B-A3B", &["--no-approve", "--no-skills"]);
    }

    /// The session a pid belongs to, read the same careful way [`alive`] reads
    /// state: `comm` can contain spaces and parentheses, so the fields are
    /// counted from the last `)`.
    #[cfg(unix)]
    fn session_of(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(3)?
            .parse()
            .ok()
    }

    /// A fresh backend on a project that has never run one has no lanes, rather
    /// than an error a pass would report as a problem every interval.
    #[test]
    fn a_project_with_no_lanes_lists_none() {
        let f = Fixture::new("empty");
        assert!(f.mux.list_lanes().unwrap().is_empty());
    }
}
