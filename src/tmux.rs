//! The tmux backend: sessions, windows and panes standing exactly where
//! herdr's workspaces, tabs and panes do.
//!
//! Everything herdr's agent layer supplied has to be built from tmux's own
//! parts. A lane's identity is a pane *user option* (`@spoolway_lane`), set
//! when the lane starts and readable by any later process — which is what lets
//! `spoolway lane -m` in a fresh terminal see the same lanes the dispatcher
//! does. A lane's status is read off its screen: a pane whose content is still
//! changing is mid-turn, one that has sat unchanged for [`Tmux::quiescence`]
//! has settled. `Blocked` is never reported — like headless, this backend
//! cannot tell a lane waiting on a person from one that merely finished, and
//! the dispatcher already routes "settled with an unchanged stage" to a person
//! by itself.
//!
//! Ids, never names: tmux session (`$n`), window (`@n`) and pane (`%n`) ids
//! are unique for the life of the server and survive renames, so they are what
//! `workspace_id`, `tab_id` and `pane_id` carry. Names are for the person
//! looking at the status bar.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::config::{DispatchConfig, MuxMode};
use crate::mux::{
    Lane, LaneSpec, LaneStatus, Mux, Workspace, branch_slug, cut_worktree,
    dispatch_workspace_label, project_label, worktree_root,
};

/// How long a pane's screen must sit unchanged before its lane counts as
/// settled. Long enough that an agent quietly reading a big file between two
/// prints is not declared done mid-turn; short enough that a finished lane is
/// picked up on the next dispatch pass or the one after.
const QUIESCENCE: Duration = Duration::from_secs(12);

/// How long a freshly started TUI is given to draw anything at all before a
/// prompt is pasted into it. The same bound herdr is given to see an agent
/// become ready, and for the same reason: a node CLI in a fresh pane on a
/// loaded machine can take well over 30s to reach its input box, and a paste
/// that lands before then is silently eaten.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a submitted prompt is given to visibly start a turn before
/// spoolway assumes the paste sat in the input box unsubmitted and presses
/// Enter once more. The same bound herdr's stall dance uses.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The pane option naming a lane: `<task> · <step>` — see
/// [`crate::mux::lane_name`]. A pane without it is not a lane — it is a
/// person's own pane, or a bare shell a session or window was just opened
/// with and nothing has started in yet — and is never counted, never
/// prompted, and above all never closed.
const OPT_LANE: &str = "@spoolway_lane";
const OPT_KIND: &str = "@spoolway_kind";

/// The worktree path the dispatcher put this lane in, stamped on every pane
/// spoolway opens — [`Tmux::split_pane`] for a split, [`Tmux::open_session`]
/// and [`Tmux::task_window`] for the initial pane a task's first step runs
/// in — and echoed back verbatim as [`Lane::cwd`]. Never
/// `#{pane_current_path}`: tmux reads that from `/proc`, symlink-resolved,
/// while this is the exact string the dispatcher recorded, so
/// [`crate::dispatch::owns_cwd`] matches it without having to canonicalise.
/// (That function does fall back to a canonical comparison, for a backend
/// like herdr that has no such stamp.)
const OPT_CWD: &str = "@spoolway_cwd";

/// The project root this lane belongs to. The default tmux server is shared by
/// everything on the machine, and two projects can both own a lane named
/// `implement-demo`; this is what keeps each dispatcher reading only its own.
const OPT_REPO: &str = "@spoolway_repo";

/// Set on a lane's first prompt: the Idle→Working boundary. A lane that has
/// it unset has been started but never prompted, which is exactly
/// [`LaneStatus::Idle`].
const OPT_PROMPTED: &str = "@spoolway_prompted";

/// The last content hash seen on this pane, and when it last changed. Written
/// by [`Tmux::list_lanes`] as it reads, and kept in tmux rather than in this
/// process so that a fresh `spoolway lane` reads the same clock the
/// dispatcher does.
const OPT_HASH: &str = "@spoolway_hash";
const OPT_ACTIVE_AT: &str = "@spoolway_active_at";

/// Marks the run's own session in `grouped` mode, so finding it again is an
/// exact test rather than a name match against whatever a person called their
/// own sessions.
const OPT_DISPATCH: &str = "@spoolway_dispatch";

/// The checkout a `split`-mode task session owns, read back by
/// [`Tmux::remove_workspace`] to take the worktree with the session. Only ever
/// set on a session whose worktree spoolway cut: a session that merely opened
/// a person's own checkout never gets it, which is what makes removal refuse.
const OPT_CHECKOUT: &str = "@spoolway_checkout";

/// The directory a session of ours was opened on, whether or not it owns it.
/// What lets a second lane for the same borrowed checkout land in the session
/// already looking at it instead of opening another beside it.
const OPT_OPENS: &str = "@spoolway_opens";

/// A private server socket a suite has pointed this backend at, in place of
/// the default one — see the `socket` field below.
///
/// Read once, in [`Tmux::new`], so every process a run spawns against tmux —
/// the dispatcher and, through the environment a lane inherits, `spoolway
/// answer` run against it later — drives the exact same scratch server the
/// suite started, rather than each one autostarting the default the moment it
/// first shells out to `tmux`. Set by `scripts/e2e/suites/disaster.sh`, and
/// by nothing in a real run: production never sets this, so `socket` stays
/// `None` and every real `Tmux` drives the default server exactly as before.
pub const ENV_SOCKET: &str = "SPOOLWAY_TMUX_SOCKET";

/// The tmux backend. Shells out to the `tmux` CLI against the default server;
/// the server is started by the first `new-session` if it is not running.
#[derive(Debug)]
pub struct Tmux {
    /// The project root: where `tmux` and `git` are invoked from, the
    /// repository every worktree is cut from, and what the run's session is
    /// named after.
    cwd: PathBuf,

    /// How this run is laid out — one session for the run, or one per task.
    mode: MuxMode,

    /// Where this project's checkouts are cut, under [`MuxMode::Split`] — see
    /// [`crate::mux::worktree_root`]. The shared session itself is opened
    /// elsewhere, on the fixed [`crate::mux::dispatch_home`].
    worktree_root: PathBuf,

    /// The bare window `new-session` had to create the run's session with,
    /// recorded only when *this* process created it. [`Tmux::move_self_into`]
    /// closes it once the dispatcher's own pane is in — and a dispatcher
    /// restarted into a session full of live lanes recorded nothing and
    /// closes nothing. The same dance herdr's `root_tab` does.
    root_window: RefCell<Option<String>>,

    /// A private server socket path, passed as `tmux -S`. `None` drives the
    /// default server, which is every real run: this is set from
    /// [`ENV_SOCKET`] when a suite names one, and by the unit tests directly,
    /// on a scratch socket of their own — both so a kill can take down the
    /// whole server on the way out without ever touching a person's own.
    socket: Option<PathBuf>,

    /// See [`QUIESCENCE`]. A field so the tests can settle in milliseconds.
    quiescence: Duration,
}

impl Tmux {
    pub fn new(cwd: &Path, config: &DispatchConfig) -> Tmux {
        Tmux {
            cwd: cwd.to_path_buf(),
            mode: config.tmux_mode,
            worktree_root: worktree_root(cwd, config),
            root_window: RefCell::new(None),
            socket: std::env::var_os(ENV_SOCKET).map(PathBuf::from),
            quiescence: QUIESCENCE,
        }
    }

    /// Run one tmux command and return its stdout.
    fn tmux(&self, args: &[&str]) -> Result<String> {
        let output = self
            .command(args)
            .output()
            .with_context(|| format!("running `tmux {}`", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "`tmux {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run one tmux command whose only interesting failure is "there is no
    /// server": `None` then, where every real error still surfaces. What lets
    /// the first `list_lanes` of a run answer "no lanes" instead of aborting
    /// the pass that was about to start the server.
    fn tmux_if_server(&self, args: &[&str]) -> Result<Option<String>> {
        let output = self
            .command(args)
            .output()
            .with_context(|| format!("running `tmux {}`", args.join(" ")))?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no server running") || stderr.contains("error connecting to") {
            return Ok(None);
        }
        bail!(
            "`tmux {}` failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        );
    }

    /// Run one tmux command with `input` on stdin. Exists for `load-buffer -`:
    /// a prompt travels as bytes on a pipe, never as an argv item and never
    /// through a file on disk.
    fn tmux_stdin(&self, args: &[&str], input: &str) -> Result<()> {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("running `tmux {}`", args.join(" ")))?;
        child
            .stdin
            .take()
            .context("no stdin handle on the tmux child")?
            .write_all(input.as_bytes())
            .context("writing to tmux stdin")?;
        let output = child
            .wait_with_output()
            .with_context(|| format!("waiting for `tmux {}`", args.join(" ")))?;
        if !output.status.success() {
            bail!(
                "`tmux {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new("tmux");
        if let Some(socket) = &self.socket {
            command.arg("-S").arg(socket);
        }
        command.args(args).current_dir(&self.cwd);
        command
    }

    /// git, in the repository this backend was built on.
    fn git(&self, args: &[&str]) -> Result<String> {
        crate::repo::run(&self.cwd, "git", args)
    }

    /// The run's own session in `grouped` mode, if it exists: exact name *and*
    /// the marker option, because the name alone is one a person could have
    /// used themselves.
    fn find_dispatch_session(&self) -> Result<Option<String>> {
        let format = format!("#{{session_id}}\t#{{session_name}}\t#{{{OPT_DISPATCH}}}");
        let Some(listing) = self.tmux_if_server(&["list-sessions", "-F", &format])? else {
            return Ok(None);
        };
        let label = session_name(&dispatch_workspace_label(&self.cwd));
        for line in listing.lines() {
            let mut fields = line.split('\t');
            let (id, name, marker) = (fields.next(), fields.next(), fields.next());
            if name == Some(label.as_str()) && marker == Some("1") {
                return Ok(id.map(str::to_string));
            }
        }
        Ok(None)
    }

    /// A window on `cwd`, in a session of ours already looking at it or a
    /// fresh one — [`Mux::create_pane`] and [`Mux::reopen_owned_pane`] both
    /// funnel through here, differing only in `owned`: whether the session
    /// gets `OPT_CHECKOUT` stamped on it, which is what tells
    /// `Mux::remove_workspace` this task's own worktree back apart from a
    /// checkout it only ever borrowed.
    fn pane_on(&self, cwd: &Path, label: &str, owned: bool) -> Result<Workspace> {
        if let Some(session) = self.session_opened_on(cwd)? {
            if owned {
                self.set_session_opt(&session, OPT_CHECKOUT, &cwd.display().to_string())?;
            }
            return self.task_window(&session, cwd, label);
        }

        let (session, window, pane) = self.open_session(label, cwd)?;
        if owned {
            self.set_session_opt(&session, OPT_CHECKOUT, &cwd.display().to_string())?;
        }
        let _ = self.tmux(&["rename-window", "-t", &window, label]);
        Ok(Workspace {
            workspace_id: session,
            pane_id: pane,
            tab_id: Some(window),
            checkout_path: cwd.to_path_buf(),
        })
    }

    /// A session of ours already opened on `path`, if one is.
    fn session_opened_on(&self, path: &Path) -> Result<Option<String>> {
        let format = format!("#{{session_id}}\t#{{{OPT_OPENS}}}");
        let Some(listing) = self.tmux_if_server(&["list-sessions", "-F", &format])? else {
            return Ok(None);
        };
        let wanted = path.display().to_string();
        for line in listing.lines() {
            if let Some((id, opens)) = line.split_once('\t')
                && opens == wanted
            {
                return Ok(Some(id.to_string()));
            }
        }
        Ok(None)
    }

    /// Open a session, stamped and with the conveniences every session of ours
    /// gets. Answers with `(session_id, window_id, pane_id)` of the initial
    /// window `new-session` cannot help creating.
    fn open_session(&self, name: &str, cwd: &Path) -> Result<(String, String, String)> {
        let path = cwd.display().to_string();
        let name = session_name(name);
        let created = self.tmux(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
            &path,
            "-P",
            "-F",
            "#{session_id}\t#{window_id}\t#{pane_id}",
        ])?;
        let created = created.trim();
        let mut fields = created.split('\t');
        let (session, window, pane) = (
            fields.next().context("new-session printed no session id")?,
            fields.next().context("new-session printed no window id")?,
            fields.next().context("new-session printed no pane id")?,
        );
        // Recorded per session rather than assumed globally: turning the
        // person's own tmux config over would be rude, but a session spoolway
        // made can be clickable and scrollable for someone who knows no
        // prefix keys yet.
        let _ = self.tmux(&["set-option", "-t", session, "mouse", "on"]);
        self.set_session_opt(session, OPT_OPENS, &path)?;
        // The same stamp [`Tmux::split_pane`] puts on a split pane, on the
        // initial pane a session opens with too: `start_lane` starts a
        // task's *first* step directly in this pane rather than splitting a
        // fresh one, and without the stamp that lane lists with an empty
        // `cwd`, the dispatcher's ownership filter drops it, and the task is
        // escalated on the next pass while its agent works. See [`OPT_CWD`].
        self.set_pane_opt(pane, OPT_CWD, &path)?;
        Ok((session.to_string(), window.to_string(), pane.to_string()))
    }

    /// A window in `session`, opened on `cwd`: the tmux shape of a herdr tab.
    fn task_window(&self, session: &str, cwd: &Path, label: &str) -> Result<Workspace> {
        let path = cwd.display().to_string();
        let target = format!("{session}:");
        let created = self.tmux(&[
            "new-window",
            "-d",
            "-t",
            &target,
            "-c",
            &path,
            "-n",
            label,
            "-P",
            "-F",
            "#{window_id}\t#{pane_id}",
        ])?;
        let created = created.trim();
        let (window, pane) = created
            .split_once('\t')
            .context("new-window printed no window and pane id")?;
        // Stamped like a split pane's — a first step started directly in
        // this pane is invisible to the dispatcher's ownership filter
        // otherwise. See [`OPT_CWD`] and [`Tmux::open_session`].
        self.set_pane_opt(pane, OPT_CWD, &path)?;
        Ok(Workspace {
            workspace_id: session.to_string(),
            pane_id: pane.to_string(),
            tab_id: Some(window.to_string()),
            checkout_path: cwd.to_path_buf(),
        })
    }

    fn set_pane_opt(&self, pane: &str, name: &str, value: &str) -> Result<()> {
        self.tmux(&["set-option", "-p", "-t", pane, name, value])?;
        Ok(())
    }

    fn set_session_opt(&self, session: &str, name: &str, value: &str) -> Result<()> {
        self.tmux(&["set-option", "-t", session, name, value])?;
        Ok(())
    }

    /// The pane a lane of ours lives in, with its window and session.
    fn lane_pane(&self, name: &str) -> Result<LanePane> {
        let format = format!("#{{{OPT_LANE}}}\t#{{{OPT_REPO}}}\t#{{pane_id}}\t#{{window_id}}");
        let repo = self.cwd.display().to_string();
        let listing = self
            .tmux_if_server(&["list-panes", "-a", "-F", &format])?
            .unwrap_or_default();
        for line in listing.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            if let [lane, lane_repo, pane, window] = fields[..]
                && lane == name
                && lane_repo == repo
            {
                return Ok(LanePane {
                    pane_id: pane.to_string(),
                    window_id: window.to_string(),
                });
            }
        }
        bail!("no lane named `{name}` is running in tmux");
    }

    /// A pane's visible text, plus up to `history` lines above it.
    fn capture(&self, pane: &str, history: usize) -> Result<String> {
        if history == 0 {
            return self.tmux(&["capture-pane", "-p", "-t", pane]);
        }
        let start = format!("-{history}");
        self.tmux(&["capture-pane", "-p", "-t", pane, "-S", &start])
    }

    /// The dispatcher's own pane, if this process is running in one on the
    /// server this backend speaks to.
    ///
    /// `$TMUX_PANE` rather than `pane current`-style asking, and unlike under
    /// herdr it stays valid: tmux pane ids survive `break-pane`, so the id the
    /// shell was started with still names this pane after it has been moved.
    /// Verified against the server rather than trusted, because the variable
    /// may name a pane of some *other* server than the one this backend uses.
    fn own_pane(&self) -> Option<OwnPane> {
        let pane = std::env::var("TMUX_PANE").ok()?;
        let answer = self
            .tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{session_id}\t#{window_id}\t#{window_panes}",
            ])
            .ok()?;
        let fields: Vec<&str> = answer.trim().split('\t').collect();
        let [session, window, panes] = fields[..] else {
            return None;
        };
        Some(OwnPane {
            pane_id: pane,
            window_id: window.to_string(),
            session_id: session.to_string(),
            window_panes: panes.parse().unwrap_or(1),
        })
    }

    /// Has the pane drawn anything yet? The gate between `respawn-pane` and
    /// the first paste: a TUI still booting eats a paste without a trace.
    fn wait_until_drawn(&self, pane: &str) -> Result<()> {
        let start = std::time::Instant::now();
        loop {
            if !self.capture(pane, 0)?.trim().is_empty() {
                return Ok(());
            }
            if start.elapsed() > READY_TIMEOUT {
                bail!(
                    "the agent in pane {pane} drew nothing in {READY_TIMEOUT:?} — it never became ready for a prompt"
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Did the pane's screen change away from `snapshot` within
    /// `SUBMIT_TIMEOUT`? A pane that vanished counts as changed: the only way
    /// it goes between the Enter and this look is the agent acting on what it
    /// was sent — exiting is a response, and one more Enter at a pane that is
    /// not there would turn a delivered prompt into an error.
    fn changed_from(&self, pane: &str, snapshot: &str) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < SUBMIT_TIMEOUT {
            match self.capture(pane, 0) {
                Ok(now) if now != snapshot => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => return true,
            }
        }
        false
    }
}

/// A lane's coordinates on the server.
struct LanePane {
    pane_id: String,
    window_id: String,
}

struct OwnPane {
    pane_id: String,
    window_id: String,
    session_id: String,
    window_panes: u32,
}

impl Mux for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn is_available(&self) -> bool {
        // The binary is enough: unlike herdr, whose server a person starts,
        // tmux starts its own on the first `new-session`.
        self.tmux(&["-V"]).is_ok()
    }

    fn unavailable(&self) -> String {
        "tmux is not installed — every lane runs in a tmux pane, so install tmux first \
         (`apt install tmux` or the platform's equivalent). To dispatch through herdr \
         instead, set `dispatch.backend = \"herdr\"`; with no multiplexer at all, \
         `\"headless\"`."
            .to_string()
    }

    fn resident_while_waiting(&self) -> bool {
        true
    }

    fn dispatch_workspace(&self, root: &Path, create: bool) -> Result<Option<String>> {
        // Nothing to find and nothing to open: `split` puts every task in a
        // session of its own and leaves the dispatcher where it was started.
        if self.mode == MuxMode::Split {
            return Ok(None);
        }
        if let Some(existing) = self.find_dispatch_session()? {
            return Ok(Some(existing));
        }
        if !create {
            return Ok(None);
        }

        // Opened on the fixed dispatch home, shared by every project, and
        // never on a checkout — see [`crate::mux::dispatch_home`].
        let home = crate::mux::dispatch_home();
        std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
        let label = dispatch_workspace_label(root);
        let (session, window, _pane) = self.open_session(&label, &home)?;
        self.set_session_opt(&session, OPT_DISPATCH, "1")?;
        // The bare shell `new-session` had to open with. Remembered so
        // [`Tmux::move_self_into`] can close exactly that window and no other.
        *self.root_window.borrow_mut() = Some(window);
        Ok(Some(session))
    }

    fn open_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<Workspace> {
        let created = self.task_window(workspace_id, cwd, label)?;

        // Now that there is a window beyond the bare one `new-session` had to
        // open with, that one can go. Only ever the window this process
        // opened: a second project finding the session already there
        // recorded no root window and closes nothing of another project's.
        if let Some(window) = self.root_window.borrow_mut().take() {
            let _ = self.tmux(&["kill-window", "-t", &window]);
        }

        Ok(created)
    }

    fn open_command(&self, cwd: &Path, label: &str, command: &str) -> Result<()> {
        // A pane exactly the way `create_pane` gives a lane one, and then the
        // command replaces its fresh shell as the pane's own process — the
        // same `respawn-pane` route `start_lane` already takes to put an
        // agent in a pane, just handed a shell command instead of the agent
        // binary directly.
        let workspace = self.create_pane(cwd, label)?;
        self.tmux(&[
            "respawn-pane",
            "-k",
            "-t",
            &workspace.pane_id,
            "sh",
            "-c",
            command,
        ])?;
        Ok(())
    }

    /// tmux's half of [`crate::mux::Mux::find_tab`]: the window of this
    /// session named `label`, by label alone — every pane in a project's
    /// window is one of its lanes now, sitting in that lane's own worktree,
    /// so there is no anchor pane left standing anywhere to check a
    /// directory against.
    fn find_tab(&self, workspace_id: &str, label: &str) -> Result<Option<String>> {
        let listing = self.tmux(&[
            "list-windows",
            "-t",
            workspace_id,
            "-F",
            "#{window_id}\t#{window_name}",
        ])?;
        for line in listing.lines() {
            let mut fields = line.split('\t');
            let (Some(window), Some(name)) = (fields.next(), fields.next()) else {
                continue;
            };
            if name == label {
                return Ok(Some(window.to_string()));
            }
        }
        Ok(None)
    }

    fn move_self_into(&self, workspace_id: &str) -> Result<()> {
        let root_window = self.root_window.borrow().clone();
        let label = project_label(&self.cwd);

        let Some(own) = self.own_pane() else {
            // Started outside tmux: there is no pane of ours to move, so the
            // shell the session was opened with is this project's own window
            // and takes its name instead of being closed.
            if let Some(window) = root_window {
                let _ = self.tmux(&["rename-window", "-t", &window, &label]);
            }
            return Ok(());
        };
        if own.session_id == workspace_id {
            return Ok(());
        }

        // A pane alone in its window cannot be broken out of it, and does not
        // need to be: the whole window moves. Focus follows either way — this
        // is the pane the person was watching, and the board they were
        // watching it for is about to draw in another session.
        let target = format!("{workspace_id}:");
        if own.window_panes <= 1 {
            self.tmux(&["move-window", "-s", &own.window_id, "-t", &target])?;
            self.tmux(&["rename-window", "-t", &own.window_id, &label])?;
        } else {
            self.tmux(&[
                "break-pane",
                "-s",
                &own.pane_id,
                "-n",
                &label,
                "-t",
                &target,
            ])?;
        }
        let _ = self.tmux(&["switch-client", "-t", workspace_id]);

        // Now that there is a second window, the bare shell the session had to
        // be created with can go. Only ever the window this process opened: a
        // dispatcher restarted into a session holding live lanes recorded no
        // root window and closes nothing.
        if let Some(window) = root_window {
            let _ = self.tmux(&["kill-window", "-t", &window]);
            *self.root_window.borrow_mut() = None;
        }
        Ok(())
    }

    fn task_owns_workspace(&self) -> bool {
        // Under `grouped` every task is a pane in the one window its project
        // shares, and that session holds the dispatcher's own pane; under
        // `split` every task is a session of its own.
        self.mode == MuxMode::Split
    }

    fn remove_checkout(&self, path: &Path) -> Result<()> {
        self.git(&["worktree", "remove", "--force", &path.display().to_string()])?;
        Ok(())
    }

    fn own_workspace(&self) -> Option<String> {
        self.own_pane().map(|own| own.session_id)
    }

    fn list_lanes(&self) -> Result<Vec<Lane>> {
        let format = format!(
            "#{{{OPT_LANE}}}\t#{{{OPT_KIND}}}\t#{{pane_id}}\t#{{window_id}}\t#{{session_id}}\
             \t#{{{OPT_CWD}}}\t#{{{OPT_REPO}}}\t#{{{OPT_PROMPTED}}}\t#{{{OPT_HASH}}}\
             \t#{{{OPT_ACTIVE_AT}}}"
        );
        // No server is no lanes, not an error: the first pass of a run asks
        // before anything has started the server, and an aborted pass would
        // stop the run from ever starting it.
        let Some(listing) = self.tmux_if_server(&["list-panes", "-a", "-F", &format])? else {
            return Ok(Vec::new());
        };

        let repo = self.cwd.display().to_string();
        let now = now_millis();
        let mut lanes = Vec::new();
        for line in listing.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            let [
                name,
                kind,
                pane,
                window,
                session,
                cwd,
                lane_repo,
                prompted,
                stored_hash,
                active_at,
            ] = fields[..]
            else {
                continue;
            };
            // A pane without a lane name is not a lane, and one stamped by
            // another project's dispatcher is not ours: the default server is
            // shared by everything on the machine. Never count either, never
            // prompt them, and above all never close their panes.
            if name.is_empty() || lane_repo != repo {
                continue;
            }

            let status = match self.capture(pane, 0) {
                Err(_) => LaneStatus::Unknown,
                Ok(screen) => {
                    let hash = content_hash(&screen);
                    let active_at = if hash != stored_hash {
                        let _ = self.set_pane_opt(pane, OPT_HASH, &hash);
                        let _ = self.set_pane_opt(pane, OPT_ACTIVE_AT, &now.to_string());
                        now
                    } else {
                        active_at.parse().unwrap_or(0)
                    };
                    if prompted.is_empty() {
                        LaneStatus::Idle
                    } else if now.saturating_sub(active_at) < self.quiescence.as_millis() as u64 {
                        LaneStatus::Working
                    } else {
                        // Settled and promptable — the resting state of every
                        // lane that has done any work, never "exited". The
                        // pane and the agent in it are still right there.
                        LaneStatus::Done
                    }
                }
            };

            lanes.push(Lane {
                name: name.to_string(),
                kind: kind.to_string(),
                status,
                pane_id: pane.to_string(),
                tab_id: window.to_string(),
                workspace_id: session.to_string(),
                cwd: PathBuf::from(cwd),
            });
        }
        Ok(lanes)
    }

    /// `has-session` and `list-windows`, never `tmux_if_server`: a session id
    /// tmux has never heard of is an ordinary non-zero exit, the same shape as
    /// "no server at all", and folding the two together would answer "gone"
    /// for a session tmux is holding right now just because the id came in
    /// wrong.
    fn workspace_alive(&self, workspace_id: &str, tab_id: Option<&str>) -> Result<bool> {
        let alive = self
            .command(&["has-session", "-t", workspace_id])
            .output()
            .with_context(|| format!("running `tmux has-session -t {workspace_id}`"))?
            .status
            .success();
        if !alive {
            return Ok(false);
        }
        let Some(tab_id) = tab_id else {
            return Ok(true);
        };
        let listing = self.tmux(&["list-windows", "-t", workspace_id, "-F", "#{window_id}"])?;
        Ok(listing.lines().any(|window| window == tab_id))
    }

    fn create_workspace(
        &self,
        _cwd: &Path,
        branch: &str,
        base: &str,
        label: &str,
    ) -> Result<Workspace> {
        // Only ever called under `split` — see [`Mux::create_workspace`] —
        // where the checkout is cut with git, into a session of its own that
        // owns it: stamped so that removal, and only removal of a session so
        // stamped, takes the worktree with it. The directory is named after
        // the branch slug, the rule every backend now shares — see
        // [`crate::mux::branch_slug`].
        let checkout = self.worktree_root.join(branch_slug(branch));
        cut_worktree(&self.cwd, &checkout, branch, base)?;

        let (session, window, pane) = self.open_session(label, &checkout)?;
        self.set_session_opt(&session, OPT_CHECKOUT, &checkout.display().to_string())?;
        let _ = self.tmux(&["rename-window", "-t", &window, label]);
        Ok(Workspace {
            workspace_id: session,
            pane_id: pane,
            tab_id: Some(window),
            checkout_path: checkout,
        })
    }

    fn remove_workspace(&self, workspace_id: &str) -> Result<()> {
        // The checkout is read back from the stamp before the session goes,
        // and a session without the stamp is refused outright: it borrowed a
        // person's own checkout, whatever the caller believes.
        let format = format!("#{{{OPT_CHECKOUT}}}");
        let checkout = self
            .tmux(&["display-message", "-p", "-t", workspace_id, &format])?
            .trim()
            .to_string();
        if checkout.is_empty() {
            bail!(
                "session {workspace_id} owns no worktree of this run's — refusing to remove \
                 the checkout it is looking at"
            );
        }
        self.tmux(&["kill-session", "-t", workspace_id])?;
        self.remove_checkout(Path::new(&checkout))
    }

    fn close_workspace(&self, workspace_id: &str) -> Result<()> {
        self.tmux(&["kill-session", "-t", workspace_id])?;
        Ok(())
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        // Refusing the last window is this backend's own job. herdr's `tab
        // close` fails on a workspace's last tab and the teardown path leans
        // on that failure to escalate; tmux's `kill-window` would instead
        // silently kill the session — which, for a window in a session a
        // person owns, is their whole session gone.
        let windows: u32 = self
            .tmux(&["display-message", "-p", "-t", tab_id, "#{session_windows}"])?
            .trim()
            .parse()
            .unwrap_or(1);
        if windows <= 1 {
            bail!("window {tab_id} is the last in its session — closing it would kill the session");
        }
        self.tmux(&["kill-window", "-t", tab_id])?;
        Ok(())
    }

    fn create_pane(&self, cwd: &Path, label: &str) -> Result<Workspace> {
        // Unstamped: the checkout is not ours to remove — see `pane_on`.
        self.pane_on(cwd, label, false)
    }

    fn reopen_owned_pane(&self, cwd: &Path, label: &str) -> Result<Workspace> {
        // Stamped, unlike `create_pane`: this checkout is spoolway's own cut,
        // only its workspace went stale, and a pane opened without the stamp
        // would leave `remove_workspace` refusing to take the worktree back —
        // see [`Mux::reopen_owned_pane`].
        self.pane_on(cwd, label, true)
    }

    fn split_pane(&self, tab_id: &str, cwd: &Path) -> Result<String> {
        let path = cwd.display().to_string();
        // Which pane in the window actually splits is tmux's own business:
        // `-t <window>` with no pane index splits the window's active pane,
        // and `select-layout tiled` below retiles every pane in it evenly
        // regardless of which one that was — spoolway needs no rule of its
        // own the way herdr does.
        let created = self.tmux(&[
            "split-window",
            "-d",
            "-t",
            tab_id,
            "-c",
            &path,
            "-P",
            "-F",
            "#{pane_id}",
        ])?;
        let pane = created.trim().to_string();
        // The exact string the dispatcher will match `Lane::cwd` against,
        // stamped rather than read back from tmux — see [`OPT_CWD`].
        self.set_pane_opt(&pane, OPT_CWD, &path)?;
        let _ = self.tmux(&["select-layout", "-t", tab_id, "tiled"]);
        Ok(pane)
    }

    fn run_in_pane(
        &self,
        tab_id: &str,
        cwd: &Path,
        label: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Option<String>> {
        // Split off the task's own window and label it before anything else
        // runs there — the same order [`Tmux::start_lane`] labels a lane's
        // pane in, and for the same reason: querying the pane again once its
        // process is running would race it exiting.
        //
        // `remain-on-exit`, unlike a lane's pane: an agent's exit is the
        // dispatcher's cue that the lane is gone and the pane should go with
        // it, but a command's own verdict is a file the wrapper writes, read
        // on a later pass — not the pane closing under it. Without this a
        // command that runs in under a second, which most do, would take its
        // own pane down before that pass ever got to read the file, and a
        // failing one would never stand for anyone to look at.
        let pane = self.split_pane(tab_id, cwd)?;
        self.rename_pane(&pane, label)?;
        self.tmux(&["set-option", "-p", "-t", &pane, "remain-on-exit", "on"])?;

        // `-e KEY=VALUE` on `respawn-pane`, the same flag [`Tmux::start_lane`]
        // sets a lane's environment with, rather than a shell export typed
        // into the pane: tmux sets it on the process this respawns directly,
        // before `script` even reads its first byte, so there is no window
        // where the script could see the environment it started with instead.
        let mut args: Vec<String> = vec!["respawn-pane".into(), "-k".into()];
        for (key, value) in env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.extend([
            "-t".into(),
            pane.clone(),
            "sh".into(),
            "-c".into(),
            script.into(),
        ]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tmux(&refs)?;
        Ok(Some(pane))
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.tmux(&["kill-pane", "-t", pane_id])?;
        Ok(())
    }

    fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()> {
        // Stamped and labelled *before* the respawn below, while the pane is
        // still its own fresh shell: a fast-exiting agent's pane is closed by
        // tmux the moment its process ends, and a real one can crash at launch
        // just as fast as a stand-in's turn does. Querying the pane again
        // afterward — for a stamp, a rename, anything — would race that exit
        // and lose on any machine slow enough for a few more `tmux` round
        // trips to outlast the process being respawned into it. Doing the
        // bookkeeping first and making the respawn itself the last call this
        // function makes needs nothing from the pane once it is issued.
        self.set_pane_opt(spec.pane_id, OPT_LANE, spec.name)?;
        self.set_pane_opt(spec.pane_id, OPT_KIND, spec.kind)?;
        self.set_pane_opt(spec.pane_id, OPT_REPO, &self.cwd.display().to_string())?;
        self.rename_pane(spec.pane_id, spec.label)?;

        // The agent replaces the pane's fresh shell as the pane's own process:
        // no keystrokes typed at a shell that may not be ready, no quoting —
        // tmux executes a multi-argument command directly. When the agent
        // exits, the pane goes with it, which is exactly how a vanished lane
        // is already read.
        let mut args: Vec<String> = vec!["respawn-pane".into(), "-k".into()];
        for (key, value) in spec.env {
            args.push("-e".into());
            args.push(format!("{key}={value}"));
        }
        args.push("-t".into());
        args.push(spec.pane_id.into());

        // A PATH prefix needs a shell to expand `$PATH`, so only then is one
        // interposed. Nothing sets it in a real run; the tests use it to put
        // a stand-in agent in front of the real one.
        match spec.path_prefix {
            Some(prefix) => {
                let shell = crate::platform::Shell::Posix;
                let exec: Vec<String> = std::iter::once(spec.kind.to_string())
                    .chain(spec.args.iter().cloned())
                    .map(|arg| shell.quote(&arg))
                    .collect();
                args.push("sh".into());
                args.push("-c".into());
                args.push(format!(
                    "{}; exec {}",
                    shell.path_export(prefix),
                    exec.join(" ")
                ));
            }
            None => {
                args.push(spec.kind.into());
                args.extend(spec.args.iter().cloned());
            }
        }

        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tmux(&args)?;
        Ok(())
    }

    fn prompt(&self, name: &str, text: &str) -> Result<()> {
        let lane = self.lane_pane(name)?;
        let pane = &lane.pane_id;

        // A TUI still booting eats a paste. herdr's `agent start` waits for
        // readiness itself; here the wait is for the screen to show anything
        // at all, bounded the same 120s.
        self.wait_until_drawn(pane)?;

        // In as a bracketed paste, so an agent that asked for bracketed paste
        // (claude and pi do) takes embedded newlines as text rather than as
        // one submission per line. `-p` only adds the bracket codes for a pane
        // whose application requested them.
        let buffer = format!("spoolway-{name}");
        self.tmux_stdin(&["load-buffer", "-b", &buffer, "-"], text)?;
        self.tmux(&["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane])?;

        // The snapshot is taken *after* the paste has landed on screen —
        // the paste itself changes the screen, so a before-snapshot would
        // read every stall as a started turn.
        std::thread::sleep(Duration::from_millis(300));
        let snapshot = self.capture(pane, 0).unwrap_or_default();
        self.tmux(&["send-keys", "-t", pane, "Enter"])?;

        // From here on the lane counts as prompted whatever happens next:
        // Idle is "never prompted", and this lane just was.
        let _ = self.set_pane_opt(pane, OPT_PROMPTED, "1");

        if self.changed_from(pane, &snapshot) {
            return Ok(());
        }

        // Stalled: the text is sitting in the input box, unsent. One more
        // Enter submits it — and only a stalled lane gets one; a turn that
        // visibly started above never sees a stray keystroke.
        self.tmux(&["send-keys", "-t", pane, "Enter"])?;
        if self.changed_from(pane, &snapshot) {
            return Ok(());
        }
        bail!("prompting `{name}`: submission stalled even after Enter")
    }

    fn read(&self, name: &str, lines: usize) -> Result<String> {
        let lane = self.lane_pane(name)?;
        let captured = self.capture(&lane.pane_id, lines)?;
        let text = captured.trim_end();
        let tail: Vec<&str> = text.lines().rev().take(lines).collect();
        Ok(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }

    fn interrupt_lane(&self, name: &str) -> Result<()> {
        let lane = self.lane_pane(name)?;
        self.tmux(&["send-keys", "-t", &lane.pane_id, "Escape"])?;
        Ok(())
    }

    fn stop_lane(&self, _name: &str, pane_id: &str) -> Result<()> {
        // Closing the pane kills the agent in it, whatever kind it is — the
        // same no-keystroke-guessing herdr settled on. The pane is the lane's
        // alone; its task's next step gets a new one.
        self.close_pane(pane_id)
    }

    // [`Mux::vacate_lane`] is deliberately *not* implemented here, and this is
    // a property of tmux rather than a gap to fill in later. `start_lane`
    // above respawns the pane with the agent as the pane's own process, so
    // there is no shell underneath it to hand back: the moment the agent
    // exits, tmux closes the pane. A kind's `quit` gesture would work exactly
    // as well here and still leave nothing standing. The trait's default —
    // close the pane, report `Vacated::PaneClosed` — is therefore the honest
    // answer, not a degraded one, and it is what tmux lanes already do.
    //
    // Making a tmux pane survive its agent would mean respawning it into a
    // shell that then execs the agent as a child, which changes how a
    // vanished lane is detected and how `interrupt_lane` reaches the process.
    // That is its own piece of work against a live tmux, not something to
    // guess at here.

    fn focus_lane(&self, name: &str) -> Result<()> {
        // Walks window and pane so anyone attached to the session is looking
        // at the lane. A client attached to some *other* session is left
        // alone: yanking a person out of unrelated work is not focusing.
        let lane = self.lane_pane(name)?;
        self.tmux(&["select-window", "-t", &lane.window_id])?;
        self.tmux(&["select-pane", "-t", &lane.pane_id])?;
        Ok(())
    }

    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.tmux(&["rename-window", "-t", tab_id, label])?;
        Ok(())
    }

    fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<()> {
        self.tmux(&["rename-session", "-t", workspace_id, &session_name(label)])?;
        Ok(())
    }

    fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()> {
        self.tmux(&["select-pane", "-t", pane_id, "-T", label])?;
        Ok(())
    }
}

/// What tmux will accept as a session name: no `.` and no `:`, which are
/// target syntax. A repository directory called `my.app` must not break the
/// run before it starts. Public because the `tmux attach` hint the dispatch
/// command prints has to name the session as tmux actually knows it.
pub fn session_name(label: &str) -> String {
    label.replace(['.', ':'], "-")
}

/// A pane's content, reduced to a short stable string. Any hash would do —
/// this one is only ever compared with itself.
fn content_hash(screen: &str) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    screen.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Is there a tmux to test against? Every test that talks to a server
    /// starts by asking, and quietly passes where there is none — the same
    /// posture the e2e suite takes toward multiplexers in CI.
    fn tmux_available() -> bool {
        std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// A backend against a private tmux server on a scratch socket, plus a
    /// `bin/` on the lane's PATH holding a stand-in agent and a real git
    /// repository to cut worktrees from.
    ///
    /// The socket is the isolation: every fixture is its own server, killed
    /// whole on drop, so the tests never see — and can never touch — a
    /// person's own sessions on the default server.
    struct Fixture {
        root: PathBuf,
        bin: PathBuf,
        repo: PathBuf,
        mux: Tmux,
    }

    impl Fixture {
        fn new(name: &str, mode: MuxMode) -> Fixture {
            let root = crate::scratch::root(&format!("tmux-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let repo = root.join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-b", "main"]);
            git(&repo, &["config", "user.email", "t@example.invalid"]);
            git(&repo, &["config", "user.name", "t"]);
            git(&repo, &["commit", "--allow-empty", "-m", "init"]);

            let mut config = DispatchConfig::default();
            config.tmux_mode = mode;
            config.worktree_root = root.join("worktrees").display().to_string();

            let mut mux = Tmux::new(&repo, &config);
            mux.socket = Some(root.join("sock"));
            // Milliseconds, so a settling lane is a fast test rather than a
            // twelve-second one.
            mux.quiescence = Duration::from_millis(800);

            Fixture {
                root,
                bin,
                repo,
                mux,
            }
        }

        /// Put a script on the lane's PATH under the name of a real agent
        /// kind, so the backend launches it exactly as it would the real
        /// thing.
        fn agent(&self, kind: &str, body: &str) -> &Fixture {
            let path = self.bin.join(kind);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::process::Command::new("chmod")
                .args(["+x", &path.display().to_string()])
                .status()
                .unwrap();
            self
        }

        /// An interactive stand-in: boots, then answers every line it is
        /// prompted with, printing while the "turn" runs. `read -r` and
        /// `printf`, so what was pasted comes back byte-for-byte.
        fn chatty_agent(&self, kind: &str) -> &Fixture {
            self.agent(
                kind,
                r#"echo booted
while read -r line; do
  [ "$line" = die ] && exit 0
  printf 'got: %s\n' "$line"
  sleep 0.2
  echo turn-over
done"#,
            )
        }

        /// A session with a pane already in it, and a lane pane split off its
        /// window — the shape a lane starts in whenever its tab already holds
        /// something else, the same way every step after a tab's first does.
        fn lane(&self, name: &str, kind: &str) -> String {
            let seed = self.mux.create_pane(&self.repo, "seed").unwrap();
            let tab = seed.tab_id.unwrap();
            let pane = self.mux.split_pane(&tab, &self.repo).unwrap();
            self.start_in(&pane, name, kind, &BTreeMap::new());
            pane
        }

        fn start_in(&self, pane: &str, name: &str, kind: &str, env: &BTreeMap<String, String>) {
            self.mux
                .start_lane(&LaneSpec {
                    name,
                    label: "implementer",
                    kind,
                    pane_id: pane,
                    args: &[],
                    env,
                    path_prefix: Some(&self.bin),
                })
                .unwrap();
        }

        fn status(&self, name: &str) -> LaneStatus {
            self.mux
                .list_lanes()
                .unwrap()
                .into_iter()
                .find(|lane| lane.name == name)
                .map(|lane| lane.status)
                .unwrap_or(LaneStatus::Unknown)
        }

        /// Block until the lane settles, so an assertion about a finished turn
        /// is never a race with output that is still arriving.
        fn settle(&self, name: &str) {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(20) {
                if self.status(name) == LaneStatus::Done {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!(
                "lane `{name}` never settled; pane:\n{}",
                self.mux.read(name, 50).unwrap_or_default()
            );
        }

        /// Block until the lane is gone from the listing — an exited agent
        /// takes its pane with it, and the close is not instantaneous.
        fn wait_gone(&self, name: &str) {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(10) {
                if !self
                    .mux
                    .list_lanes()
                    .unwrap()
                    .iter()
                    .any(|lane| lane.name == name)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("lane `{name}` never left the listing");
        }

        fn sessions(&self) -> Vec<String> {
            self.mux
                .tmux_if_server(&["list-sessions", "-F", "#{session_id}"])
                .unwrap()
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.mux.tmux(&["kill-server"]);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Two fixtures built with the same name in one process must not share a
    /// root — that collision is exactly what let one run's setup
    /// `remove_dir_all` another run's still-live socket directory out from
    /// under it. Doesn't need a live tmux server, so it runs unconditionally
    /// rather than behind `tmux_available`.
    #[test]
    fn two_fixtures_with_the_same_name_get_different_roots() {
        let a = Fixture::new("same-name", MuxMode::Split);
        let b = Fixture::new("same-name", MuxMode::Split);
        assert_ne!(a.root, b.root);
        assert_ne!(a.mux.socket, b.mux.socket);
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?} failed");
    }

    /// The whole lifecycle of one lane against a real pane: written down but
    /// unprompted is idle, printing is working, and quiet — with the agent
    /// alive at its prompt — is done. Done is the resting state, not an exit:
    /// the trait doc warns that reading it as "exited" strands every lane
    /// after its first turn.
    #[test]
    fn a_lane_is_idle_until_prompted_then_working_then_done() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("lifecycle", MuxMode::Split);
        f.chatty_agent("pi");
        f.lane("implement-demo", "pi");

        assert_eq!(
            f.status("implement-demo"),
            LaneStatus::Idle,
            "started but never prompted"
        );

        f.mux.prompt("implement-demo", "go").unwrap();
        assert_eq!(
            f.status("implement-demo"),
            LaneStatus::Working,
            "the turn is visibly running"
        );

        f.settle("implement-demo");
        let lanes = f.mux.list_lanes().unwrap();
        let lane = lanes.iter().find(|l| l.name == "implement-demo").unwrap();
        assert_eq!(lane.kind, "pi");
        assert_eq!(
            lane.cwd, f.repo,
            "the cwd is the stamped path, exactly as the dispatcher passed it"
        );
        assert!(
            f.mux
                .read("implement-demo", 10)
                .unwrap()
                .contains("got: go"),
            "the prompt arrived and was answered"
        );
    }

    /// The initial pane a session opens with carries the cwd stamp too, not
    /// only a split pane. A task's first step runs directly in this pane;
    /// unstamped, it lists with an empty `cwd`, the dispatcher's ownership
    /// filter drops it, and the task is escalated while its agent works. See
    /// review finding 1.
    #[test]
    fn the_initial_pane_of_a_session_carries_the_cwd_stamp() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("initial-pane-cwd", MuxMode::Split);
        f.chatty_agent("pi");
        let ws = f
            .mux
            .create_workspace(&f.repo, "task/demo", "main", "implementer")
            .unwrap();
        f.start_in(&ws.pane_id, "implement-demo", "pi", &BTreeMap::new());

        let lanes = f.mux.list_lanes().unwrap();
        let lane = lanes
            .iter()
            .find(|l| l.name == "implement-demo")
            .expect("the lane is listed");
        assert!(
            !lane.cwd.as_os_str().is_empty(),
            "an unstamped initial pane lists with an empty cwd"
        );
        assert_eq!(
            lane.cwd, ws.checkout_path,
            "the initial pane is stamped with the same path a split one is"
        );
    }

    /// A second prompt reaches the same session: the agent is resident, and
    /// answering it resumes the conversation rather than starting one.
    #[test]
    fn a_settled_lane_can_be_prompted_again() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("reprompt", MuxMode::Split);
        f.chatty_agent("pi");
        f.lane("implement-demo", "pi");

        f.mux.prompt("implement-demo", "first").unwrap();
        f.settle("implement-demo");
        f.mux.prompt("implement-demo", "second").unwrap();
        f.settle("implement-demo");

        let tail = f.mux.read("implement-demo", 30).unwrap();
        assert!(tail.contains("got: first"), "{tail}");
        assert!(tail.contains("got: second"), "{tail}");
    }

    /// An agent that exits takes its pane with it, and the lane leaves the
    /// listing — which is how the dispatcher already reads a vanished lane.
    #[test]
    fn an_exited_agent_takes_its_pane_and_lane_with_it() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("exit", MuxMode::Split);
        f.chatty_agent("pi");
        f.lane("implement-demo", "pi");

        f.mux.prompt("implement-demo", "die").unwrap();
        f.wait_gone("implement-demo");
    }

    /// Shell metacharacters in a prompt arrive as text, not as shell. The
    /// paste path never goes through a shell at all, which is the point.
    #[test]
    fn a_hostile_prompt_arrives_verbatim() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("hostile", MuxMode::Split);
        f.chatty_agent("pi");
        f.lane("implement-demo", "pi");

        let hostile = r#"fix this; rm -rf $HOME && echo "done" 'or not' `id`"#;
        f.mux.prompt("implement-demo", hostile).unwrap();
        f.settle("implement-demo");
        assert!(
            f.mux
                .read("implement-demo", 20)
                .unwrap()
                .contains(&format!("got: {hostile}")),
            "the prompt must arrive byte-for-byte:\n{}",
            f.mux.read("implement-demo", 20).unwrap()
        );
    }

    /// The lane's environment rides in on `respawn-pane -e` and is there
    /// before the agent's first line runs.
    #[test]
    fn the_environment_reaches_the_agent() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("env", MuxMode::Split);
        f.agent(
            "pi",
            "echo task:$SPOOLWAY_TASK\nwhile read -r line; do :; done",
        );
        let seed = f.mux.create_pane(&f.repo, "seed").unwrap();
        let pane = f.mux.split_pane(&seed.tab_id.unwrap(), &f.repo).unwrap();
        let env = BTreeMap::from([("SPOOLWAY_TASK".to_string(), "demo-42".to_string())]);
        f.start_in(&pane, "implement-demo", "pi", &env);

        let started = std::time::Instant::now();
        loop {
            let tail = f.mux.read("implement-demo", 10).unwrap_or_default();
            if tail.contains("task:demo-42") {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "env never showed: {tail}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A pane without the lane stamp is somebody else's, whatever is running
    /// in it — never listed, never prompted, never closed.
    #[test]
    fn a_foreign_pane_is_not_a_lane() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("foreign", MuxMode::Split);
        f.mux.create_pane(&f.repo, "somebody-elses").unwrap();
        assert!(f.mux.list_lanes().unwrap().is_empty());
        assert!(f.mux.read("implement-demo", 10).is_err());
    }

    /// Stopping a lane closes its pane and delists it, and closing a pane that
    /// never held a lane is just as fine.
    #[test]
    fn stop_lane_kills_the_pane() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("stop", MuxMode::Split);
        f.chatty_agent("pi");
        let pane = f.lane("implement-demo", "pi");

        f.mux.stop_lane("implement-demo", &pane).unwrap();
        f.wait_gone("implement-demo");

        let spare = f.mux.create_pane(&f.repo, "spare").unwrap();
        let bare = f.mux.split_pane(&spare.tab_id.unwrap(), &f.repo).unwrap();
        f.mux.close_pane(&bare).unwrap();
    }

    /// Interrupting a lane reaches its pane — unlike stopping it, the session
    /// survives: the lane is still listed, still there for a later resume to
    /// find exactly as it left it.
    #[test]
    fn interrupt_lane_reaches_the_pane_without_ending_the_session() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("interrupt", MuxMode::Split);
        f.chatty_agent("pi");
        f.lane("implement-demo", "pi");

        f.mux.interrupt_lane("implement-demo").unwrap();

        assert!(
            f.mux
                .list_lanes()
                .unwrap()
                .iter()
                .any(|lane| lane.name == "implement-demo"),
            "an interrupt must leave the session for a later resume to find"
        );
    }

    /// `grouped`: one shared session, found again rather than remade, one
    /// window per project holding a pane per task, and nothing of a task's
    /// own to tear down beyond its checkout.
    #[test]
    fn grouped_mode_is_one_session_with_a_pane_per_task() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("grouped", MuxMode::Grouped);
        assert!(!f.mux.task_owns_workspace());

        let session = f.mux.dispatch_workspace(&f.repo, true).unwrap().unwrap();
        assert_eq!(
            f.mux.dispatch_workspace(&f.repo, true).unwrap().unwrap(),
            session,
            "a second ask finds the session the first one opened"
        );

        let project_tab = f.mux.open_tab(&session, &f.repo, "spoolway").unwrap();
        assert_eq!(project_tab.workspace_id, session);
        let tab_id = project_tab.tab_id.unwrap();

        // A task under `grouped` cuts its own checkout with git directly,
        // never through `create_workspace` — see [`Mux::create_workspace`] —
        // and its lane runs in a pane split off the project's own tab.
        let checkout = f.root.join("worktrees").join("demo");
        crate::mux::cut_worktree(&f.repo, &checkout, "task/demo", "main").unwrap();
        assert!(checkout.is_dir(), "the worktree was cut");
        let pane = f.mux.split_pane(&tab_id, &checkout).unwrap();

        // Tearing the task down closes only its pane and its checkout — the
        // project's tab is not a task's to close.
        f.mux.close_pane(&pane).unwrap();
        f.mux.remove_checkout(&checkout).unwrap();
        assert!(!checkout.exists());

        // What is left is the project's own tab; closing the last window
        // must refuse rather than take the session with it.
        let err = f.mux.close_tab(&tab_id).unwrap_err();
        assert!(err.to_string().contains("last"), "{err}");
        assert_eq!(f.sessions(), vec![session.clone()], "the session survived");

        f.mux.close_workspace(&session).unwrap();
        assert!(f.sessions().is_empty());
    }

    /// `split`: a session per task owning its checkout, and removal takes
    /// both — while a session that merely borrowed a checkout is refused.
    #[test]
    fn split_mode_is_a_session_per_task_and_removal_is_owned_only() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("split", MuxMode::Split);
        assert!(f.mux.task_owns_workspace());
        assert!(f.mux.dispatch_workspace(&f.repo, true).unwrap().is_none());

        let task = f
            .mux
            .create_workspace(&f.repo, "task/demo", "main", "spoolway/demo")
            .unwrap();
        assert!(task.checkout_path.is_dir());
        f.mux.remove_workspace(&task.workspace_id).unwrap();
        assert!(
            !task.checkout_path.exists(),
            "the worktree went with the session"
        );
        assert!(f.sessions().is_empty());

        // A borrowed checkout: the session is ours, the directory is not.
        let borrowed = f.mux.create_pane(&f.repo, "closeout").unwrap();
        let err = f.mux.remove_workspace(&borrowed.workspace_id).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
        assert!(f.repo.is_dir(), "the person's checkout is untouched");
        assert_eq!(f.sessions().len(), 1, "and their session still stands");
    }

    /// A worktree healed by [`crate::dispatch::ensure_workspace`]'s heal
    /// path — its session gone, its directory still standing — has to answer
    /// to `remove_workspace` exactly as the session that first cut it did.
    /// `reopen_owned_pane` is what makes that true: the plain `create_pane`
    /// this used to go through left the reopened session unstamped, so
    /// `remove_workspace` refused it and the worktree leaked.
    #[test]
    fn a_healed_owned_pane_is_still_removable() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("heal-owned", MuxMode::Split);
        let task = f
            .mux
            .create_workspace(&f.repo, "task/demo", "main", "spoolway/demo")
            .unwrap();

        // The session is gone — a restart, say — but the worktree it cut
        // survives, exactly what the heal path finds.
        f.mux.close_workspace(&task.workspace_id).unwrap();
        assert!(
            task.checkout_path.is_dir(),
            "the worktree outlives its session"
        );

        let healed = f
            .mux
            .reopen_owned_pane(&task.checkout_path, "spoolway/demo")
            .unwrap();

        f.mux.remove_workspace(&healed.workspace_id).unwrap();
        assert!(
            !task.checkout_path.exists(),
            "a healed pane's worktree must go with its session exactly like a freshly cut one's"
        );
    }

    /// A second lane against the same borrowed checkout lands in the session
    /// already looking at it, not in a new one beside it.
    #[test]
    fn a_borrowed_checkout_is_opened_once() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("borrowed", MuxMode::Split);
        let first = f.mux.create_pane(&f.repo, "review").unwrap();
        let second = f.mux.create_pane(&f.repo, "closeout").unwrap();
        assert_eq!(first.workspace_id, second.workspace_id);
        assert_ne!(first.tab_id, second.tab_id, "each got a window of its own");
    }

    /// A command step's own pane: split off an existing window, running the
    /// script handed to it rather than a bare shell, and labelled the way
    /// `command_step::Runs::key` names it.
    #[test]
    fn run_in_pane_splits_and_runs_the_script() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("run-in-pane", MuxMode::Split);
        let seed = f.mux.create_pane(&f.repo, "seed").unwrap();
        let tab = seed.tab_id.unwrap();

        let marker = f.root.join("ran.txt");
        let script = format!("echo hello-from-the-pane | tee {}", marker.display());
        let pane = f
            .mux
            .run_in_pane(&tab, &f.repo, "demo · build", &script, &BTreeMap::new())
            .unwrap()
            .expect("tmux always has a pane to offer");

        let started = std::time::Instant::now();
        loop {
            if marker.exists() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the script never ran"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            "hello-from-the-pane"
        );

        let title = f
            .mux
            .tmux(&["display-message", "-p", "-t", &pane, "#{pane_title}"])
            .unwrap();
        assert_eq!(title.trim(), "demo · build");
    }

    /// The environment argument reaches the script, and a name it shares with
    /// one the pane already inherited is decided by the argument, not by
    /// whatever the process the pane started from happened to carry —
    /// `respawn-pane -e` sets it on the process this call starts directly,
    /// which is why this is the one road there is no window to lose the race
    /// on.
    #[test]
    fn run_in_pane_hands_a_named_value_over_an_inherited_one() {
        if !tmux_available() {
            return;
        }
        // SAFETY: this test's own tmux server is spawned below, on its own
        // scratch socket, and inherits this process's environment at that
        // moment — nothing else reads this key.
        unsafe {
            std::env::set_var("SPOOLWAY_TMUX_ENV_TEST", "inherited");
        }
        let f = Fixture::new("run-in-pane-env", MuxMode::Split);
        let seed = f.mux.create_pane(&f.repo, "seed").unwrap();
        let tab = seed.tab_id.unwrap();
        unsafe {
            std::env::remove_var("SPOOLWAY_TMUX_ENV_TEST");
        }

        let marker = f.root.join("env.txt");
        let script = format!(
            "echo \"$SPOOLWAY_TMUX_ENV_TEST\" | tee {}",
            marker.display()
        );
        let mut env = BTreeMap::new();
        env.insert("SPOOLWAY_TMUX_ENV_TEST".to_string(), "named".to_string());
        f.mux
            .run_in_pane(&tab, &f.repo, "demo · env", &script, &env)
            .unwrap();

        let started = std::time::Instant::now();
        loop {
            if marker.exists() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the script never ran"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "named");
    }

    /// Renames are cosmetic and must never fail a pass.
    #[test]
    fn renames_touch_labels_not_identity() {
        if !tmux_available() {
            return;
        }
        let f = Fixture::new("rename", MuxMode::Split);
        let ws = f.mux.create_pane(&f.repo, "demo · implement").unwrap();
        f.mux
            .rename_workspace(&ws.workspace_id, "demo · review")
            .unwrap();
        f.mux
            .rename_tab(ws.tab_id.as_deref().unwrap(), "demo · review")
            .unwrap();
        f.mux.rename_pane(&ws.pane_id, "reviewer").unwrap();
        // Renaming to something tmux would refuse as a session name must
        // still work: the sanitizer owns the difference.
        f.mux
            .rename_workspace(&ws.workspace_id, "v2.0: retry")
            .unwrap();
    }

    #[test]
    fn a_session_name_is_never_target_syntax() {
        assert_eq!(session_name("my.app-dispatcher"), "my-app-dispatcher");
        assert_eq!(session_name("v2.0: retry"), "v2-0- retry");
        assert_eq!(session_name("plain-name"), "plain-name");
    }

    /// The one mode vocabulary parses in both spellings: the new one, and the
    /// herdr-era one every existing config on disk still uses.
    // covers: dispatch.herdr_mode — the layout a run is given, and the older spellings of it that still parse
    #[test]
    fn mode_aliases_keep_old_configs_parsing() {
        let old: DispatchConfig =
            toml::from_str("herdr_mode = \"workspace\"\ntmux_mode = \"worktrees\"").unwrap();
        assert_eq!(old.herdr_mode, MuxMode::Grouped);
        assert_eq!(old.tmux_mode, MuxMode::Split);

        let new: DispatchConfig =
            toml::from_str("herdr_mode = \"split\"\ntmux_mode = \"grouped\"").unwrap();
        assert_eq!(new.herdr_mode, MuxMode::Split);
        assert_eq!(new.tmux_mode, MuxMode::Grouped);

        // `worktree` joins `worktrees` as a spelling that parses as `split` —
        // a third sidebar mode was never added, only a second alias for one
        // that already exists.
        let singular: DispatchConfig = toml::from_str("herdr_mode = \"worktree\"").unwrap();
        assert_eq!(singular.herdr_mode, MuxMode::Split);
    }
}
