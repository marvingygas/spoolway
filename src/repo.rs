//! Locating the repo spoolway is operating on, and the thin git wrapper every
//! other module goes through.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::platform::PathExt;
use crate::task::{self, Task};

/// A repo with spoolway state in it: the root directory, its loaded config,
/// and the project's own home directory.
#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    /// The checkout `root` was discovered from — the linked worktree itself
    /// when a lane runs in one, `root` again in the main checkout. `root`
    /// still names the one project every command's queue, lane state and
    /// lock agree on; `checkout` is where the tracked control plane —
    /// pipelines, prompts, task skeletons — is actually read from, so a
    /// lane validates its own branch's files rather than the main
    /// checkout's. See [`Repo::prompts_dir`] and friends.
    pub checkout: PathBuf,
    pub config: Config,
    /// `~/.spoolway/<label>-<id>/` — where every runtime file this project's
    /// spoolway writes actually lives, `<label>` and `<id>` being
    /// [`crate::mux::project_home`]'s own answer for `root`. See
    /// [`Repo::home`] for why this is a field rather than a method computed
    /// from `root` on every call: a test fixture sets it to a scratch
    /// directory directly, so nothing under test ever depends on the real
    /// `$HOME`.
    pub home: PathBuf,
}

impl Repo {
    /// Find the project a command is operating on.
    ///
    /// A task worktree is a checkout of the same repo and carries `.spoolway/`
    /// with it — config, the pipeline and prompts are all tracked. So walking
    /// up for a `.spoolway` directory finds the *worktree*, whose `queue/` is
    /// empty because task files are deliberately untracked. A lane calling
    /// `spoolway report` from there would never find its own task.
    ///
    /// Ask git instead: from a linked worktree, the common git dir points at
    /// the main checkout, which is the project.
    pub fn discover(start: &Path) -> Result<Repo> {
        let start = start
            .canonical()
            .with_context(|| format!("resolving {}", start.display()))?;
        let main = main_checkout(&start);
        let root = Repo::root(&start, main.as_deref())?;
        let checkout = checkout_for(&start, &root, main.as_deref());
        let config = Config::load(&root)?;
        // Checked here, once, rather than in every accessor that creates a
        // directory under a home on demand: `bind` is what decides which
        // `~/.spoolway/<name>/` this checkout is allowed to write into,
        // proceeding, recording a move, or refusing outright — see `bind`'s
        // own doc for the seven states this settles between. `doctor` takes
        // the same fact as a finding through `discover_lenient`'s
        // `bind_lenient`, and a test fixture that sets `home` by hand never
        // comes through here.
        let home = bind(&root)?;
        Ok(Repo {
            root,
            checkout,
            config,
            home,
        })
    }

    /// The project, plus the reason its config could not be read, if it could
    /// not be read.
    ///
    /// `doctor` is the reason this exists — every other command is right to
    /// die on a config it cannot parse, but `doctor` is the command you
    /// reach for *because* the config is wrong, so it takes the parse error
    /// as a finding rather than a reason not to start — but `main` reaches
    /// for it from several other places too, wherever opening a broken file
    /// is the point (`config edit`, `config override`) or a best-effort
    /// answer beats a hard failure (`agent verify`, the update-check
    /// notice). The defaults `Repo.config` gets in place of a config that
    /// would not parse are not reported on by any of them: they exist so
    /// there is a `Repo` to hang whatever each caller can still do without
    /// one.
    ///
    /// The stamped home is read the same tolerant way, for the same reason
    /// — a permissions problem or a corrupt repository resolving it must
    /// not stop discovery either — through
    /// [`crate::mux::project_home_lenient`]. The two errors are not
    /// independent in one direction: `Config::load` resolves the overrides
    /// layer through the same call `project_home_lenient` just failed, so a
    /// broken home is read here with `Config::load_tracked` instead,
    /// keeping `config_error` a fact about the tracked file alone rather
    /// than a second name for the same failure. A broken config with a
    /// perfectly good home is the ordinary, independent case this still
    /// covers.
    pub fn discover_lenient(
        start: &Path,
    ) -> Result<(Repo, Option<anyhow::Error>, Option<anyhow::Error>)> {
        let start = start
            .canonical()
            .with_context(|| format!("resolving {}", start.display()))?;
        let main = main_checkout(&start);
        let root = Repo::root(&start, main.as_deref())?;
        let checkout = checkout_for(&start, &root, main.as_deref());
        let (home, home_error) = bind_lenient(&root);
        // `Config::load` resolves the overrides layer through
        // `crate::mux::project_home` — the same call `home_error` above
        // just failed — so calling it the ordinary way here would fail
        // again for the identical reason and report one real problem
        // (a broken home) as two (a broken home *and* a broken config).
        // Read the tracked file alone when that has already happened; only
        // when the home resolved is a config failure worth telling apart
        // from it at all.
        let loaded = if home_error.is_some() {
            Config::load_tracked(&root)
        } else {
            Config::load(&root)
        };
        match loaded {
            Ok(config) => Ok((
                Repo {
                    root,
                    checkout,
                    config,
                    home,
                },
                None,
                home_error,
            )),
            Err(err) => Ok((
                Repo {
                    root,
                    checkout,
                    config: Config::default(),
                    home,
                },
                Some(err),
                home_error,
            )),
        }
    }

    /// The project directory `start` belongs to, before anything is read out of
    /// it. `start` is already canonicalized, and `main` is `main_checkout`'s
    /// own answer for it — both callers resolve each once and share them with
    /// [`checkout_of`], so this makes no `git rev-parse` call that a caller
    /// has not already paid for.
    fn root(start: &Path, main: Option<&Path>) -> Result<PathBuf> {
        if let Some(main) = main.filter(|dir| crate::config::setup_dir_in(dir).is_dir()) {
            return Ok(main.to_path_buf());
        }

        // The walk stops at the checkout's own top. Left unbounded, it
        // climbed out of the repository and on up to `$HOME`, where the
        // global `~/.spoolway/` — every project's state, not a project's
        // control plane — read as a `.spoolway` directory of its own, and a
        // `queue add` run on a branch without `.spoolway/` silently queued
        // into `~/.spoolway/<user>/`. Outside any repository there is no top
        // to stop at, and the two identity checks below are what stand
        // between the walk and that same directory.
        let top = git_toplevel(start).ok();
        let state_root = global_state_root();
        let found = start
            .ancestors()
            .take_while(|dir| top.as_deref().is_none_or(|top| dir.starts_with(top)))
            .filter(|dir| *dir != state_root && crate::config::setup_dir_in(dir) != state_root)
            .find(|dir| crate::config::setup_dir_in(dir).is_dir());
        if let Some(dir) = found {
            return Ok(dir.to_path_buf());
        }

        // No `.spoolway/` anywhere in the checkout. That used to fall back to
        // the toplevel with a default config, which is how a checkout on the
        // wrong branch became a project with no files in it. Two answers now,
        // both errors: a checkout that *is* a registered project has its
        // `.spoolway/` on some other branch, and is told so by name; anything
        // else was never initialised.
        if let Some(top) = top.as_deref() {
            let project = main.unwrap_or(top);
            // Best-effort: this only sharpens the error below into a more
            // specific one, so a resolution failure here just falls through
            // to the generic bail rather than replacing it with a different
            // error about a question nobody asked.
            if let Some(pointer) = crate::mux::project_home(project)
                .ok()
                .and_then(|home| read_binding(&home).ok().flatten())
                .map(|binding| binding.root)
                && (pointer == project || pointer == top)
            {
                bail!(
                    "{} is a spoolway project, but `.spoolway/` is not on branch `{}` — check \
                     out a branch that carries it, or run `spoolway init` here",
                    top.display(),
                    branch_or_detached(top),
                );
            }
        }
        // Nothing here names *this* checkout by any workspace that could be
        // read — but before falling through to the generic "no spoolway
        // project found", ask once more, fallibly: a broken workspace file
        // is exactly how that generic message gets reached for a checkout
        // that *is* listed, just in a `project.toml` nobody could read. That
        // deserves its own error naming the file, not the same "run
        // `spoolway init`" that would convert this very clone to repo mode.
        //
        // A match here, rather than an error, means the opposite kind of
        // gap: `start` is listed, its workspace's `project.toml` reads
        // fine, but `setup_dir_in` above still found no directory — the
        // workspace's own `config/` is missing. Naming that folder here,
        // rather than falling through, is what stops `spoolway init` from
        // reading this the same as a checkout nobody has ever set up and
        // writing a fresh default config into a workspace other clones
        // still share.
        if let Some(clone) = workspace_clone_checked(start)? {
            bail!(
                "{} is listed as a clone of {}, but {} does not exist\n  restore it by hand, or \
                 remove this checkout's entry from {} by hand to leave the workspace",
                start.display(),
                clone.workspace.display(),
                clone.config_dir().display(),
                clone.workspace.join(BINDING_FILE).display(),
            );
        }

        // Nothing here names *this* checkout — but a workspace elsewhere on
        // this machine may still be naming one that moved or was deleted
        // without being re-attached. Each workspace `start` would actually
        // take such an entry's queue over in — see `stale_workspace_clones`
        // for the exact rule — gets its own `init --workspace` line; see
        // the `home-mode-discovery` task's non-goal on not going further
        // than reporting one.
        let stale = stale_workspace_clones(start);
        if stale.is_empty() {
            bail!(
                "no spoolway project found at or above {} (run `spoolway init` there first)",
                start.display()
            )
        }
        bail!(
            "no spoolway project found at or above {}\n  run `spoolway init` there first — or, \
             if this is a clone that moved, re-attach it:\n{}",
            start.display(),
            stale
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Whether `err` is one of [`Repo::root`]'s "nothing claims this
    /// checkout" refusals — the two bails just above that share the "no
    /// spoolway project found" sentence, plus [`workspace_clone_checked`]'s
    /// own bail, which reaches here through `workspace_clone_checked(start)?`
    /// in `Repo::root` when no readable workspace lists `start` and a broken
    /// workspace file elsewhere on the machine leaves undecided whether it
    /// would have. That third case is swallowed because the answer is
    /// unknown, not because it is known to be "no": the broken file might
    /// be exactly the one naming this checkout. What `config path` owes a
    /// person then is the list with that file in it, carrying its own
    /// `error` field, so they can see which workspace to repair.
    ///
    /// Every other error `Repo::discover` can return (a `.spoolway/` on
    /// another branch, a listed clone whose own folder is gone, two
    /// workspaces both claiming this root, …) is a case where something
    /// readable does name this checkout, and each needs a person's action
    /// `config path` has no business papering over — which is why only
    /// these three are swallowed here.
    ///
    /// `commands::config_path_anywhere` is the one caller that treats this
    /// as an answer — `mode: null` plus the workspace list — rather than a
    /// hard failure.
    pub(crate) fn is_unclaimed(err: &anyhow::Error) -> bool {
        let message = err.to_string();
        message.starts_with("no spoolway project found at or above")
            || message.starts_with("this checkout is in no workspace spoolway can read, and ")
    }

    /// Whether work here stops for a person: what the run in progress was
    /// started as, and the project's own setting when there is no run.
    ///
    /// The run wins because `spoolway dispatch --unattended` is a decision about
    /// *tonight*, taken after config.toml was last written, and every lane it
    /// starts has to take the same one — a lane that parked itself on `blocked`
    /// inside a run whose dispatcher never stops for anybody is a task nothing
    /// will ever come back to.
    pub fn unattended(&self) -> bool {
        crate::lock::Lock::unattended(&self.lock_file()).unwrap_or(self.config.unattended.enabled)
    }

    /// Does this repo have a remote to push to and fetch from at all?
    pub fn has_remote(&self) -> bool {
        self.git(&["remote"])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
    }

    /// Whether `origin` has `branch`, asked of the remote directly rather
    /// than of this checkout's own remote-tracking refs — a branch nobody
    /// here has ever fetched still answers `git ls-remote` honestly, which
    /// is the whole point of accepting a base only `origin` has ever seen.
    ///
    /// Git's own credential prompt is turned off: a private remote this
    /// process has no terminal to answer for must fail outright rather than
    /// hang a queue submission or a dispatcher start on a password nobody
    /// is there to type.
    pub fn remote_branch_exists(&self, branch: &str) -> bool {
        if !self.has_remote() {
            return false;
        }
        // The full ref, never the bare name: `ls-remote` reads a pattern as
        // a glob matched against the tail of a ref, starting at a `/`
        // boundary, so a bare `gh-412-checkout` would answer `true` for a
        // real `refs/heads/task/gh-412-checkout` it never named at all
        // (review finding 1).
        let full_ref = format!("refs/heads/{branch}");
        let output = Command::new("git")
            .args(["ls-remote", "--heads", "origin", &full_ref])
            .current_dir(&self.root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
        matches!(output, Ok(o) if o.status.success() && !o.stdout.is_empty())
    }

    /// The checkout that has `branch` out, if any.
    ///
    /// The main checkout is one entry among the worktrees here, so a plan
    /// branch is found the same way whether it is the branch the dispatcher was
    /// started on or one of several beside it.
    pub fn worktree_for(&self, branch: &str) -> Result<Option<PathBuf>> {
        let listing = self.git(&["worktree", "list", "--porcelain"])?;
        let wanted = format!("refs/heads/{branch}");

        let mut path: Option<PathBuf> = None;
        for line in listing.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(rest.trim()));
            } else if let Some(reference) = line.strip_prefix("branch ")
                && reference.trim() == wanted
            {
                return Ok(path);
            }
        }
        Ok(None)
    }

    /// `~/.spoolway/<label>-<id>/` — every runtime file this project's
    /// spoolway writes: the queue, the archive, pending tasks, scratch
    /// worktrees, composed prompts, the headless backend's records, command-step logs,
    /// `lanes.json`, `usage.jsonl`, `dispatch.pid`, and the two scratch queue
    /// indexes.
    ///
    /// Created silently the moment anything is asked to resolve under it —
    /// a fresh clone nobody has bound to a home yet gets one, empty, rather
    /// than failing or printing a note nobody asked for: an empty queue is
    /// the honest answer for a machine that has run nothing yet. A home
    /// deleted by hand out from under an *already bound* checkout is a
    /// different case, caught earlier — `bind`, called from
    /// [`Repo::discover`] before a `Repo` exists to call this on, refuses a
    /// valid stamp with no home behind it rather than quietly recreating
    /// one for a checkout something else may still be recording.
    ///
    /// Deliberately outside the checkout. `queue/` and `archive/` are this
    /// machine's in-flight work, not the project's tracked control plane —
    /// `config.toml`, `pipelines/`, `prompts/`, `templates/tasks/`, all still
    /// under [`crate::config::STATE_DIR`] in the checkout — and a clone or a
    /// tarball of the repo was never meant to carry a queue with it.
    pub fn home(&self) -> &Path {
        let _ = std::fs::create_dir_all(&self.home);
        &self.home
    }

    /// A subdirectory of [`Repo::home`], created silently if it is missing.
    fn home_subdir(&self, name: &str) -> PathBuf {
        let dir = self.home().join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    pub fn queue_dir(&self) -> PathBuf {
        self.home_subdir(crate::config::QUEUE_DIR)
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.home_subdir(crate::config::ARCHIVE_DIR)
    }

    /// Where a producer leaves tasks for the queue screen to list:
    /// one flat directory of `.md` files, no subdirectories and no index.
    ///
    /// Created silently like every other sibling, so a project that has
    /// planned nothing still has a real, empty directory to read rather than
    /// a missing-path error on the first draw. Tasks live here only until
    /// they are queued — the screen deletes a group's files by the same act
    /// that writes its task files — so an empty directory is the ordinary
    /// resting state, not a sign anything is wrong.
    pub fn pending_dir(&self) -> PathBuf {
        self.home_subdir(crate::config::PENDING_DIR)
    }

    /// Where the short-lived worktrees a rebase needs are cut, one per task.
    pub fn scratch_dir(&self) -> PathBuf {
        self.home_subdir(crate::config::SCRATCH_DIR)
    }

    /// One composed system prompt per lane, named `<task> · <step>.md` —
    /// alongside, whenever a herdr pane needed one, the `<task> · <step>.env`
    /// that pane sourced its environment from rather than had it typed in;
    /// see [`crate::mux::Herdr::start_lane`].
    pub fn system_prompts_dir(&self) -> PathBuf {
        self.home_subdir(crate::dispatch::SYSTEM_PROMPTS_DIR)
    }

    /// The headless backend's own bookkeeping: lane records, logs and pids.
    pub fn headless_dir(&self) -> PathBuf {
        self.home_subdir(crate::headless::LANE_DIR)
    }

    /// What a `commands:` pipeline step leaves behind: each run's log, pid and
    /// exit code.
    pub fn commands_dir(&self) -> PathBuf {
        self.home_subdir(crate::command_step::RUN_DIR)
    }

    /// What the `[issue_tracking]` hook leaves behind: each event's own log,
    /// pid and exit code, one set per task per event — see [`crate::tracking`].
    pub fn tracking_dir(&self) -> PathBuf {
        self.home_subdir(crate::tracking::TRACKING_DIR)
    }

    /// Every directory [`crate::retain`]'s sweep may delete an old entry
    /// from under `housekeeping.retention_days`. [`Repo::archive_dir`] is
    /// deliberately not here: finished tasks age under their own key,
    /// `housekeeping.archive_retention_days`, which defaults to keeping them.
    /// The one place that split is written down as code — see
    /// [`crate::retain`]'s own doc for why `queue_dir`, `pending_dir`,
    /// `crate::mux::worktree_root` and `plans/` are never in this list.
    ///
    /// [`Repo::system_prompts_dir`] is here and [`Repo::prompts_dir`] is
    /// deliberately not. They read alike and mean opposite things: the first
    /// is one composed prompt per lane, machine state under [`Repo::home`],
    /// rewritten before every launch. The second is `.spoolway/prompts/` in
    /// the checkout — the project's tracked prompt templates, which
    /// [`crate::version`] fingerprints as part of how work is done. Sweeping
    /// that one deletes checked-in files off an ordinary long-lived working
    /// tree, because a file git has not rewritten in a month is a month old.
    pub fn byproduct_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.system_prompts_dir(),
            self.commands_dir(),
            self.tracking_dir(),
            self.headless_dir(),
            self.scratch_dir(),
        ]
    }

    /// `.spoolway/` under `checkout` — the branch actually running, not
    /// necessarily `root`'s. The one accessor every tracked-setup path below
    /// is built from; [`crate::config::setup_dir_in`] is its free-function
    /// twin for a caller (`Config::load`, `Pipelines::load`, …) that only has
    /// a bare checkout path and not yet a `Repo` to ask.
    pub fn setup_dir(&self) -> PathBuf {
        crate::config::setup_dir_in(&self.checkout)
    }

    /// [`crate::config::under_setup`], against this `Repo`'s own
    /// [`Repo::setup_dir`] — `commands::init` reaches for the free function
    /// directly instead, against the `setup_dir_in(root)` it already
    /// resolved, since it has no whole `Repo` yet to ask.
    pub(crate) fn under_setup(&self, full: &str) -> PathBuf {
        crate::config::under_setup(&self.setup_dir(), full)
    }

    /// The tracked control plane's own directories, read from `checkout` —
    /// the branch actually running, not necessarily `root`'s.
    pub fn prompts_dir(&self) -> PathBuf {
        self.under_setup(crate::config::PROMPTS_DIR)
    }

    /// The optional patch layer's own directory — see [`crate::overrides`].
    ///
    /// Under [`Repo::home`], never `checkout`: the merge it feeds has to
    /// answer the same way whichever worktree it runs from, and `home` is
    /// already keyed on the main checkout's basename regardless. Not a
    /// `home_subdir` — it is never auto-created the way `queue_dir` and
    /// `pending_dir` are, since a project that has overridden nothing has no
    /// `overrides/` at all, which is how "off" is spelled; only a promote or
    /// create command (not this task's) ever writes into it.
    pub fn overrides_dir(&self) -> PathBuf {
        self.home.join(crate::config::OVERRIDES_DIR)
    }

    /// The private layer's own directory — see [`crate::local`]. Under
    /// [`Repo::home`], the same as [`Repo::overrides_dir`] and for the same
    /// reason, and read only in repo mode: [`crate::local::is_repo_mode`] is
    /// every caller's to check first, since `home` still resolves to a real
    /// directory in home mode and this alone would not say so.
    pub fn local_dir(&self) -> PathBuf {
        self.home.join(crate::local::LOCAL_DIR)
    }

    /// Where a project keeps tasks it wants to re-run — see
    /// [`crate::config::ROUTINES_DIR`] for why nothing auto-creates this the
    /// way [`Repo::pending_dir`] creates itself: an ordinary project that has
    /// saved no routine has no directory here at all, and the routines tab
    /// says so by naming this path rather than opening an empty one.
    pub fn routines_dir(&self) -> PathBuf {
        self.under_setup(crate::config::ROUTINES_DIR)
    }

    pub fn task_templates_dir(&self) -> PathBuf {
        self.under_setup(crate::config::TASK_TEMPLATES_DIR)
    }

    /// The project-scoped cron-job store, tracked in the checkout — read from
    /// `checkout` like every other tracked file above, so a lane sees its own
    /// branch's jobs. See [`crate::jobs`].
    pub fn jobs_file(&self) -> PathBuf {
        self.under_setup(crate::config::JOBS_FILE)
    }

    /// Every task's lane state, across every dispatcher this machine has run
    /// for this project.
    pub fn lanes_file(&self) -> PathBuf {
        self.home().join(crate::dispatch::LANES_FILE)
    }

    /// One mark per task a dispatcher is booting a lane for right now — see
    /// [`crate::claim`]. Not a `home_subdir`: nothing needs it to exist
    /// until a mark is written, and writing one creates it.
    pub fn claims_dir(&self) -> PathBuf {
        self.home().join(crate::claim::CLAIMS_DIR)
    }

    /// The usage ledger every lane's turn appends a line to.
    pub fn usage_file(&self) -> PathBuf {
        self.home().join(crate::usage::LEDGER_FILE)
    }

    /// The user-scoped cron-job store, in this machine's per-project home and
    /// never tracked — the default place a job is written. See [`crate::jobs`].
    pub fn user_jobs_file(&self) -> PathBuf {
        self.home().join(crate::config::JOBS_STORE)
    }

    /// When each job last fired, always in machine home whichever store the
    /// job itself came from — a fired minute is a fact about this machine,
    /// not about the project. See [`crate::jobs`].
    pub fn jobs_state_file(&self) -> PathBuf {
        self.home().join(crate::jobs::STATE_FILE)
    }

    /// The advisory lock over the usage ledger's read-diff-append — see
    /// [`crate::lock::LedgerLock`]. Held only around banking, so two
    /// `spoolway` commands catching the same interactive session up cannot
    /// both diff against the same banked total and append the same delta.
    pub fn ledger_lock_file(&self) -> PathBuf {
        self.home()
            .join(format!("{}.lock", crate::usage::LEDGER_FILE))
    }

    /// The advisory lock over every writer of `archive/index.jsonl` — see
    /// [`crate::lock::ArchiveIndexLock`]. Beside the archive rather than in
    /// it, because a file created inside `archive/` would move the folder's
    /// modification time that the index is compared against.
    pub fn archive_index_lock_file(&self) -> PathBuf {
        crate::archive_index::lock_file_for(&self.archive_dir())
    }

    /// The running dispatcher's own lock, if there is one.
    pub fn lock_file(&self) -> PathBuf {
        self.home().join(crate::lock::LOCK_FILE)
    }

    /// The advisory lock over one task file's read-modify-write, shared by
    /// the dispatcher and that task's own `spoolway report` — see
    /// [`crate::lock::TaskLock`]. The caller is responsible for `id` being a
    /// safe single path segment: the dispatcher only ever passes an id that
    /// has already been through `check_id`, and `spoolway report --task`
    /// joins the same unvalidated id here that `Repo::task` already joins.
    pub fn task_lock_file(&self, id: &str) -> PathBuf {
        self.home().join("task-locks").join(format!("{id}.lock"))
    }

    /// Bare `spoolway`'s own lock: written when the screen opens, and
    /// removed when it quits — see [`crate::lock::SCREEN_LOCK_FILE`]. A
    /// second `spoolway` or `spoolway dispatch` in the same project refuses
    /// while this, or [`Repo::lock_file`], names a live process.
    pub fn screen_lock_file(&self) -> PathBuf {
        self.home().join(crate::lock::SCREEN_LOCK_FILE)
    }

    /// Every active task, in id order.
    ///
    /// A `*.md` in the queue that will not parse is skipped rather than
    /// failing the whole read — see [`task::load_dir`]. Callers that want to
    /// name the bad file reach for [`Repo::tasks_and_problems`] instead.
    pub fn tasks(&self) -> Result<Vec<Task>> {
        Ok(task::load_dir(&self.queue_dir())?.0)
    }

    /// Every active task in id order, together with the queue files that
    /// would not parse — for the dispatcher pass, which names the bad file
    /// where a person will see it rather than leaving it only in the log.
    /// The board reads the queue through `status::cached_queue` instead, a
    /// per-file cache over the same [`task::load_dir`] that skips reparsing a
    /// file whose bytes have not moved since the last frame.
    pub fn tasks_and_problems(&self) -> Result<(Vec<Task>, Vec<task::LoadProblem>)> {
        task::load_dir(&self.queue_dir())
    }

    /// The id of every task file currently in `queue/`, read from the file
    /// names alone rather than by parsing each one.
    ///
    /// Two callers want exactly this, cheaply and without a parse skipping a
    /// task whose file happens not to load: [`crate::retain`]'s sweep, so it
    /// never ages out the scratch directory or headless record of a task
    /// still in flight — `paused` and `blocked` are stages a task can sit on
    /// for longer than `housekeeping.retention_days` — and [`crate::tracking::failure_count`],
    /// so the board stops counting a hook failure once its task is archived.
    pub fn queued_ids(&self) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        let Ok(entries) = std::fs::read_dir(self.queue_dir()) else {
            return ids;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("md")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                ids.insert(stem.to_string());
            }
        }
        ids
    }

    /// Find a task by id in the queue, then in the archive.
    pub fn task(&self, id: &str) -> Result<Task> {
        let queue = self.queue_dir();
        let archive = self.archive_dir();
        task::find(&[&queue, &archive], id)
    }

    /// The branch a dependency's work lives on, read from that task's own
    /// `branch:` field rather than rebuilt from its id. `queue add` may stamp
    /// `task/<slug>-<dep_id>` when `issue_tracking.key_in_names` is enabled,
    /// so rebuilding `task/<dep_id>` can name a ref that does not exist — a
    /// failure that surfaces at the dependent's worktree cut rather than here,
    /// where the name was chosen.
    ///
    /// A dependency whose task file is there but cannot be loaded is an
    /// error that names the dependency, not a `task/<dep_id>` guess handed
    /// on to a cut that then fails over a ref nobody can place. A file that
    /// is *gone* is different: the branch may well still be there, and it is
    /// asked for by its default name before this gives up — see the body.
    pub fn dependency_branch(&self, dep_id: &str) -> Result<String> {
        let file = format!("{dep_id}.md");
        let on_disk = [self.queue_dir(), self.archive_dir()]
            .iter()
            .any(|dir| dir.join(&file).exists());
        if !on_disk {
            // `retain` sweeps the archive purely by age, with no regard for a
            // queued dependent still naming the swept task — so a dependent
            // parked for longer than `retention.days` comes back to find its
            // dependency's file gone while the branch it has to be cut from
            // is still there. The file only said which branch that was. A
            // prefixed `task/<slug>-<id>` cannot be rebuilt without it, but
            // the default can, so that one is asked for, and this refuses
            // only when it does not exist either (lifecycle review finding 5).
            let branch = task::default_branch(dep_id);
            let full = format!("refs/heads/{branch}");
            if self
                .git(&["rev-parse", "--verify", "--quiet", &full])
                .is_ok()
            {
                return Ok(branch);
            }
            bail!(
                "The branch of dependency `{dep_id}` could not be resolved: its task file is \
                 in neither this project's queue nor its archive, and no `{branch}` branch \
                 exists. Add its task file back to the queue or archive before running a \
                 task that depends on it."
            );
        }
        // The wrapped error from [`Repo::task`] already says how the file
        // fails to parse; this only adds why it matters and what to do.
        let dep = self.task(dep_id).with_context(|| {
            format!(
                "The branch of dependency `{dep_id}` could not be resolved. Repair its task \
                 file before running a task that depends on it."
            )
        })?;
        Ok(dep
            .front
            .branch
            .clone()
            .unwrap_or_else(|| task::default_branch(dep_id)))
    }

    /// The branch this checkout is on.
    pub fn branch(&self) -> Result<String> {
        branch_at(&self.root)
    }

    /// The `checkout:` fact a command that reads the tracked control plane
    /// prints above its own output — `None` when `checkout` and `root` name
    /// the same directory, which is the common case: almost every command
    /// runs in the main checkout, where saying so would only be noise.
    ///
    /// Built once, here, so the line and its `--json` twin ([`CheckoutNote`])
    /// always agree — both are read off the same [`branch_at`] call rather
    /// than each call site asking git again.
    pub fn checkout_note(&self) -> Result<Option<CheckoutNote>> {
        if self.checkout == self.root {
            return Ok(None);
        }
        Ok(Some(CheckoutNote {
            path: self.checkout.clone(),
            branch: branch_at(&self.checkout)?,
        }))
    }

    /// Run a git command in the repo root, returning stdout on success.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, "git", args)
    }
}

/// The fact [`Repo::checkout_note`] found: the checkout's own absolute path
/// and the branch it has out.
///
/// The one struct feeding both forms a reader of a command's output can ask
/// for — the `checkout:` line drawn in the mockup, and the `--json` twin
/// that carries the same two facts as fields instead of prose. See
/// [`CheckoutNote::print`].
#[derive(Debug, Clone, Serialize)]
pub struct CheckoutNote {
    pub path: PathBuf,
    pub branch: String,
}

impl CheckoutNote {
    /// Print the note in whichever form the command was asked to answer in.
    ///
    /// Plain: `checkout: <path, shortened to ~> (<branch>)`, matching the
    /// mockup exactly. `--json`: the same two facts as one line of JSON,
    /// with the path left absolute — a script reading it has no `$HOME` to
    /// resolve `~` against, and no reason to be handed one that does.
    pub fn print(&self, json: bool) -> Result<()> {
        if json {
            println!("{}", serde_json::to_string(self)?);
        } else {
            println!("checkout: {} ({})", shorten_home(&self.path), self.branch);
        }
        Ok(())
    }
}

/// `path`, with a leading run matching the home directory rewritten to `~` —
/// the short form the `checkout:` line prints, and the one `init` and `sync`
/// name a home-mode workspace's files and user-level skills by. Left absolute when `path` does
/// not sit under home, or when home cannot be resolved at all.
pub(crate) fn shorten_home(path: &Path) -> String {
    let Some(home) = crate::platform::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The main checkout of the repo containing `dir`.
///
/// In a linked worktree, `--git-common-dir` resolves to the main checkout's
/// `.git`. Its parent *usually* is that checkout — true whenever `.git`
/// sits directly inside it, the ordinary layout — but not always: see
/// [`recorded_or_parent`] for the `--separate-git-dir`/submodule case this
/// verifies first rather than assumes. In the main checkout the two git
/// dirs are the same and this returns it unchanged.
///
/// `pub(crate)` rather than private: `crate::commands::dispatch::check_backend_checkout`
/// calls this a second time, on [`Repo::checkout`] as well as [`Repo::root`],
/// to name the main checkout in its own refusal when herdr cannot take the
/// dispatch checkout as `--cwd` — a checkout that is itself a linked worktree
/// (`.git` a file, not a directory), which herdr refuses with
/// `linked_worktree_source`. `Repo::root` usually already names the main
/// checkout, but a project whose `.spoolway/` sits inside a linked worktree
/// finds that worktree first, through [`Repo::root`]'s own ancestor search —
/// which is the other of `check_backend_checkout`'s two calls.
pub(crate) fn main_checkout(dir: &Path) -> Option<PathBuf> {
    // Pre-existing behaviour, unchanged by the `Result` `common_git_dir` now
    // carries: every caller of `main_checkout` already treats any failure to
    // resolve as "not a linked worktree of anything", so a real error here
    // collapses to `None` exactly as it always did. `recorded_or_parent`
    // answering `None` is folded into that same case rather than guessed
    // past with a wrong path — see its own doc for why a guess here would be
    // worse than admitting there is no answer yet.
    common_git_dir(dir)
        .ok()
        .flatten()
        .and_then(|common| recorded_or_parent(&common))
}

/// The checkout a common git directory belongs to: the common directory's
/// own parent, verified, or whatever [`stamped_id`] recorded there when the
/// parent is not it, or the workspace clone entry sharing this common
/// directory (see [`listed_checkout_of`]). `None` when none of the three
/// holds — a fresh `--separate-git-dir` clone or submodule nothing has
/// stamped or listed yet.
///
/// The parent is right for the ordinary layout, where `.git` sits directly
/// inside the checkout — but *verified*, not assumed, by asking what
/// `common_git_dir` answers for the parent in turn: a renamed or copied
/// checkout must resolve fresh off wherever it is now (the id and label
/// stay frozen at their first stamp, but a *location* has to track the
/// truth or a copy carrying a stale recorded path would read as the
/// original it was copied from) rather than trusting whatever `stamped_id`
/// wrote the last time this checkout's common directory was reachable from
/// its own parent.
///
/// The parent is wrong, and the recorded path is the only answer there is,
/// for a `--separate-git-dir` clone or a submodule: there the common
/// directory can sit anywhere at all, and git itself tracks no path back
/// from it to the checkout, so the parent is not even a git repository —
/// `common_git_dir` on it answers `None`, or `Some` of some unrelated
/// repository's own common directory, never this one's.
///
/// Returning the *unverified* parent as a last resort used to be this
/// function's own fallback — wrong for exactly the case it exists to fix:
/// the very first `spoolway init` in a fresh `--separate-git-dir` clone,
/// before anything is recorded, would resolve to the git directory's own
/// parent (wherever `--separate-git-dir` happened to point) rather than the
/// checkout, stamping the wrong folder and refusing to ever stamp the real
/// one. `None` here instead lets every caller fall back to whatever it
/// already does when a checkout carries no linked-worktree relationship at
/// all — `main_checkout`'s own callers, and [`crate::main::init_root`],
/// already treat that case correctly.
fn recorded_or_parent(common: &Path) -> Option<PathBuf> {
    if let Some(parent) = common.parent()
        && common_git_dir(parent).ok().flatten().as_deref() == Some(common)
    {
        return Some(parent.to_path_buf());
    }
    read_recorded_root(common).or_else(|| listed_checkout_of(common))
}

/// The checkout a workspace's `project.toml` lists whose common git
/// directory is `common` — how a home-mode `--separate-git-dir` clone's
/// linked worktrees find their main checkout. Home mode stamps nothing into
/// a clone, so [`read_recorded_root`] has nothing to read there, and
/// matching the worktree's own path against the clone entries can never
/// hit: the entry names the main checkout. Seen 2026-10-01: `spoolway init
/// --setup home` in such a clone worked from the checkout, and every lane
/// worktree cut from it got "no spoolway project found".
///
/// Only reached once the parent check and the recorded path have both
/// failed, so the one `git rev-parse` per listed clone is paid by that rare
/// layout alone. Best-effort like [`workspace_clone`]: a broken workspace
/// file reads as "not listed", and [`Repo::root`]'s own last-resort
/// `workspace_clone_checked` is what reports it.
fn listed_checkout_of(common: &Path) -> Option<PathBuf> {
    all_workspaces()
        .unwrap_or_default()
        .0
        .into_iter()
        .flat_map(|(_, toml)| toml.clones)
        .map(|clone| clone.root)
        .find(|root| common_git_dir(root).ok().flatten().as_deref() == Some(common))
}

/// Whether `dir` is inside a linked worktree rather than a main checkout:
/// git's own `--git-dir` for it differs from its `--git-common-dir`. False
/// for anything git cannot answer about — a non-git folder is no linked
/// worktree of anything.
///
/// Asked of git rather than read off [`main_checkout`], because this is for
/// the one case `main_checkout` answers `None` for a real repository: a
/// worktree of a `--separate-git-dir` clone nothing has stamped or listed,
/// where `crate::main::init_root` must refuse rather than set the worktree
/// up as a project of its own.
pub(crate) fn is_linked_worktree(dir: &Path) -> bool {
    let ask = |flag: &str| {
        run(dir, "git", &["rev-parse", "--path-format=absolute", flag])
            .ok()
            .and_then(|out| PathBuf::from(out.trim()).canonical().ok())
    };
    match (ask("--git-dir"), ask("--git-common-dir")) {
        (Some(own), Some(common)) => own != common,
        _ => false,
    }
}

/// What [`stamped_id`] last recorded as `common`'s checkout — `None` when
/// nothing has stamped it yet, or the recorded path no longer exists (a
/// stale record is no better an answer than the parent trick already
/// falls back to).
fn read_recorded_root(common: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(common.join(ROOT_FILE)).ok()?;
    let path = PathBuf::from(raw.trim());
    path.is_dir().then_some(path)
}

/// The common git directory itself behind `dir` — `Ok(None)` for a bare
/// repo, or for a directory with no git repository behind it at all.
///
/// Pulled out of [`main_checkout`] because [`stamped_id`] needs the directory
/// itself, not its parent: the stamp file lives inside it, in the one place
/// shared by every branch, subdirectory and linked worktree of the clone —
/// see the task context this implements, `binding-stamp`.
///
/// `Ok(None)` only for the three facts that really mean "there is nothing to
/// stamp here": `dir` does not exist at all, git says it is not inside a
/// repository, or it is a bare one with no checkout to speak of. Anything
/// else that goes wrong — `git` missing from `PATH`, a permissions problem,
/// canonicalizing failing — is `Err`, never folded into that same `None`:
/// [`crate::commands::init::init`] reads `None` as "run `git init` here
/// first", which would be a lie about a real repository that merely could
/// not be resolved this time.
fn common_git_dir(dir: &Path) -> Result<Option<PathBuf>> {
    // Checked before ever shelling out: a directory that does not exist at
    // all fails `Command::output`'s own `current_dir` at the OS level, whose
    // error carries no git wording for the "not a git repository" match
    // below to find — and plenty of tests exercise pure path arithmetic
    // (`worktree_root`, `project_label`) against a path that was never
    // created on disk, which must read as "no repository", not as a failure.
    if !dir.exists() {
        return Ok(None);
    }
    let common = match run(
        dir,
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        Ok(out) => out,
        Err(err) => {
            // git's own wording for "there is nothing here at all" —
            // matched as text because `run` hands back a failure as a
            // formatted message, not a structured reason. Anything git
            // fails with that does not say this is a real error instead.
            if format!("{err:#}").contains("not a git repository") {
                return Ok(None);
            }
            return Err(err);
        }
    };
    let common = PathBuf::from(common.trim());
    // A bare repo has no checkout to speak of — a real fact about it, not a
    // failure to resolve one. Asked of git directly, not guessed from the
    // common directory's name: a `--separate-git-dir` clone can name its
    // git directory anything at all (`repos/foo.git`, a bare `elsewhere`,
    // …), and a name-based guess read every one of those as "not a git
    // repository" — refusing a real repository for a "has no git
    // repository behind it" reason that was simply false.
    // The first `run` above already proved `git` runs and `dir` is inside a
    // repository, so a failure here is a real one — the same `Err`, never
    // `None`, every other unexpected failure in this function already is.
    let is_bare = run(dir, "git", &["rev-parse", "--is-bare-repository"])?.trim() == "true";
    if is_bare {
        return Ok(None);
    }
    // Canonicalized so it compares against the `start` every caller has
    // already canonicalized — a symlinked checkout otherwise compares
    // unequal to itself.
    let canonical = common
        .canonical()
        .with_context(|| format!("resolving the git directory for {}", dir.display()))?;
    Ok(Some(canonical))
}

/// `dir`'s first commit, recorded on a [`CloneEntry`] as a repository
/// fingerprint when it is written — `None` when `dir` carries no commits
/// yet, is not a git checkout at all, `git` could not be asked, or `dir` is
/// a shallow clone.
///
/// A shallow clone's own "first commit" is really just its fetch boundary,
/// not the repository's actual root: `git clone --depth 1` of this very
/// checkout reports a different value here than this checkout itself does,
/// even though it is the same repository. Recording that boundary would
/// poison the entry for every later takeover; comparing against it (see
/// [`commit_exists`]) would refuse the very re-clone that made it. Checked
/// first, before `rev-list` ever runs, so neither ever happens.
pub(crate) fn root_commit(dir: &Path) -> Option<String> {
    if run(dir, "git", &["rev-parse", "--is-shallow-repository"])
        .is_ok_and(|out| out.trim() == "true")
    {
        return None;
    }
    run(dir, "git", &["rev-list", "--max-parents=0", "HEAD"])
        .ok()
        .and_then(|out| out.lines().next().map(str::to_string))
}

/// `dir`'s `origin` remote URL, trimmed — `None` with no `origin` remote, no
/// git repository at all, or `git` could not be asked.
fn origin_url(dir: &Path) -> Option<String> {
    let url = run(dir, "git", &["remote", "get-url", "origin"]).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_string())
}

/// Whether `dir`'s repository actually holds `commit` — what
/// [`join_workspace`]'s different-repository refusal checks a checkout
/// taking a gone entry's dispatcher over against, rather than comparing
/// two [`root_commit`] values for equality.
///
/// Equality would be wrong on its own: a repository with more than one root
/// commit (history merged in from elsewhere) can print a different first
/// line from `rev-list` depending on which branch `HEAD` happens to be on,
/// so the *same* checkout can disagree with its own earlier fingerprint.
/// Asking whether the recorded commit is simply present answers the
/// question a takeover actually needs — is this really the repository the
/// entry was set up for — without caring which commit `rev-list` would
/// have picked first.
fn commit_exists(dir: &Path, commit: &str) -> bool {
    run(
        dir,
        "git",
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    )
    .is_ok()
}

/// The file names a project's identity is stamped under, inside its common
/// git directory — see [`stamped_id`] and [`project_identity`].
const ID_FILE: &str = "spoolway-id";
const LABEL_FILE: &str = "spoolway-label";
/// Where [`stamped_id`] records the checkout it was called on, inside the
/// same common git directory — the one thing neither `.git`'s own layout nor
/// any `git rev-parse` flag answers for a `--separate-git-dir` clone or a
/// submodule, where the common directory need not sit inside the checkout
/// at all, so its parent is not reliably the checkout. See [`main_checkout`].
const ROOT_FILE: &str = "spoolway-root";

/// How many characters a stamped id is drawn to. Six lowercase-alphanumeric
/// characters is short enough to read comfortably in a directory name
/// (`api-k7f2q9`) while 36^6 (~2.2 billion) keeps two projects landing on the
/// same one a non-event.
const ID_LEN: usize = 6;

const ID_ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Stamp `dir`'s common git directory with an id and a label, minting
/// whichever of the two is not already there — the one place either is ever
/// written. Read back unchanged on a repeat call, so running `spoolway init`
/// again is idempotent.
///
/// `Ok(None)` only when `dir` has no git repository behind it at all: that
/// is the one case [`crate::commands::init::init`] turns into a refusal
/// rather than falling back to a basename-keyed home. Two callers reach
/// this now — `init` itself, and [`bind_unstamped`], which mints the same
/// way when a command finds a checkout that carries no stamp and nothing
/// else records it (acceptance criterion 7 of the `binding-record` task).
/// Every other reader of a project's identity goes through
/// [`project_identity`], which only ever reads what this has already
/// written, and mints nothing: a plain `git clone` carries no
/// `.git/spoolway-id` of its own (it is not a tracked file), so a second
/// clone stays unresolvable — refused by [`bind`] naming both files — until
/// something actually mints it a stamp of its own.
///
/// The `bool` is whether *this call* is the one that minted the id — read
/// straight off [`read_or_mint`]'s own atomic result, not a separate
/// existence check made before or after it that a second racing process
/// could invalidate. `commands::init::init` uses it to decide whether the
/// mockup's "stamped" line has anything to say: a repeat `init` reads the
/// same id back and says nothing.
pub fn stamped_id(dir: &Path) -> Result<Option<(String, bool)>> {
    let Some(common) = common_git_dir(dir)? else {
        return Ok(None);
    };
    // `dir` *is* the checkout being stamped — every caller passes the
    // directory `spoolway init` (or `bind_unstamped`) is actually running
    // on. Deriving it instead from `common`'s parent used to be how this
    // read back a different directory entirely for a `--separate-git-dir`
    // clone or a submodule, where the common git directory need not sit
    // inside the checkout at all.
    let checkout = dir
        .canonical()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let (id, minted) = read_or_mint(&common.join(ID_FILE), is_valid_id, generate_id)?;
    // Established alongside the id, not read back until later: `stamped_id`
    // is the one call that is allowed to write at all, so this is where the
    // label this project's home will carry forever gets fixed too.
    // `sanitize_label` rather than the raw basename: a Unix checkout can be
    // named anything but `/` and NUL, and a name `is_valid_label` refuses
    // (a raw `\`, say) must not be committed unchanged — `read_or_mint`
    // would write it, `project_identity` would then refuse to read it back,
    // and every caller would silently fall back to the basename despite a
    // file calling itself the stamp sitting right there in `.git`.
    read_or_mint(&common.join(LABEL_FILE), is_valid_label, || {
        sanitize_label(&crate::mux::project_label(&checkout))
    })?;
    // Recorded fresh on every call, unlike the id and label above: this is
    // a locator, not an identity, and it is the only way a later call made
    // from a linked worktree — whose own `--git-common-dir` points at this
    // same file — can find the checkout again when `common`'s parent does
    // not name it. See [`main_checkout`].
    crate::task::write_atomic(&common.join(ROOT_FILE), checkout.display().to_string())?;
    Ok(Some((id, minted)))
}

/// Where [`stamped_id`] reads and writes `dir`'s id — `.git/spoolway-id` in
/// the ordinary case. `Ok(None)` under the same condition [`stamped_id`]
/// answers `Ok(None)`.
///
/// `pub(crate)` for `commands::init::init`'s own report of what it stamped —
/// nothing else needs the path itself rather than just the id.
pub(crate) fn id_file_path(dir: &Path) -> Result<Option<PathBuf>> {
    Ok(common_git_dir(dir)?.map(|common| common.join(ID_FILE)))
}

/// The three facts [`crate::mux::project_home`] keys a project's home on:
/// the checkout its stamp lives in — the main checkout, whichever of its
/// branches, subdirectories or linked worktrees `dir` names — the label
/// frozen the moment it was first stamped, and the id stamped alongside it.
///
/// Read-only, unlike [`stamped_id`]: `Ok(None)` both when `dir` has no git
/// repository behind it, and when it does but nothing has stamped it yet —
/// `project_home` falls back to `dir`'s own current basename either way,
/// matching what every caller always got before this stamp existed. This
/// function itself never *creates* a stamp merely by being asked about
/// one — `Repo::root`'s own "is this checkout registered under a different
/// name" nicety, `usage::registry` and every other plain lookup all read
/// through here (or through [`crate::mux::project_home`], which calls it)
/// and mint nothing, so running any of them against an unrelated git
/// repository never silently writes into its `.git`. `Repo::discover` is
/// the one deliberate exception: it goes through [`bind`] instead, which
/// mints a stamp on purpose for a checkout carrying no id and nothing
/// recording it (acceptance criterion 7 of `binding-record`) — a choice
/// `bind` makes explicitly, never a side effect of calling this.
pub fn project_identity(dir: &Path) -> Result<Option<(PathBuf, String, String)>> {
    let Some(common) = common_git_dir(dir)? else {
        return Ok(None);
    };
    // `recorded_or_parent` only fails to resolve a checkout that has never
    // been stamped — the same checkout `peek` below would answer `None` for
    // anyway, since `stamped_id` always writes the recorded path alongside
    // the id and label in the one call that writes either. Folded into the
    // same "nothing stamped here yet" answer rather than guessed past.
    let Some(checkout) = recorded_or_parent(&common) else {
        return Ok(None);
    };
    let Some(id) = peek(&common.join(ID_FILE), is_valid_id)? else {
        return Ok(None);
    };
    let Some(label) = peek(&common.join(LABEL_FILE), is_valid_label)? else {
        return Ok(None);
    };
    Ok(Some((checkout, label, id)))
}

/// The file a project's home holds recording which checkout it belongs to,
/// and which id that checkout was carrying the last time the two were
/// checked against each other — see [`bind`].
pub(crate) const BINDING_FILE: &str = "project.toml";

/// What a home's `project.toml` says: the id its checkout was stamped with,
/// and the checkout itself. The two files that must agree — the checkout's
/// own `.git/spoolway-id` and this — are read and reconciled together only
/// by [`bind`], and nothing else ever writes this file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Binding {
    id: String,
    root: PathBuf,
}

/// The binding recorded at `home`, if there is one readable. `Ok(None)` only
/// for "nothing written there yet" (a missing file); a file that exists but
/// will not parse is a real error, never silently treated the same as no
/// record at all — that distinction is exactly what tells acceptance
/// criterion 4 (no record) apart from a corrupted one, which this project
/// leaves for a person to look at rather than guessing past.
fn read_binding(home: &Path) -> Result<Option<Binding>> {
    let path = home.join(BINDING_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let binding: Binding =
                toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(binding))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// The id and root `home`'s own `project.toml` records, for a caller
/// outside this module that wants to say more than just "this home
/// exists". Two callers: `doctor`'s own binding check, and
/// `usage::registry::list`, which trusts this over its own last-registered
/// guess whenever a home has a real record — see that module's doc for
/// why. `None` collapses two different facts for both of them: no record
/// at all, and one that exists but will not parse. Neither caller treats
/// that as an error worth surfacing on its own — `doctor` reads its own
/// `home_error` for the distinction instead, and `registry::list` simply
/// falls back to its last-registered root — so a corrupt `project.toml`
/// reads the same as an absent one here rather than failing either
/// caller's own read. This is only ever asked about a home `bind` has
/// already settled on, or one `registry` once registered a root under.
pub(crate) fn binding_at(home: &Path) -> Option<(String, PathBuf)> {
    read_binding(home).ok().flatten().map(|b| (b.id, b.root))
}

/// Write `binding` to `home`'s `project.toml`, whole — the atomic write
/// `spoolway init`'s old pointer file already used, reused here since this
/// replaces it. The one place either a fresh binding or a moved one
/// (acceptance criteria 2 and 7) is written.
fn write_binding(home: &Path, binding: &Binding) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let body = format!(
        "# Which checkout this directory holds the state of, and the id its\n\
         # `.git` is stamped with. Checked against each other on every\n\
         # command — see the `binding-record` task. Updated on its own only\n\
         # to record a checkout that moved (the one it named is gone, or no\n\
         # longer carries this id).\n{}",
        toml::to_string_pretty(binding).context("serialising project.toml")?
    );
    crate::task::write_atomic(&home.join(BINDING_FILE), body)
}

/// How a checkout's own stamp read, for [`bind`] to react to each
/// differently. [`peek`] alone collapses "missing" and "wrong format" into
/// one `None`, which is right for [`project_identity`]'s silent basename
/// fallback but wrong here: a wrong-format stamp is a fact worth its own
/// refusal (acceptance criterion 5), not read the same as nothing stamped
/// at all.
enum Stamp {
    None,
    Invalid(String),
    Valid(String),
}

/// Read `root`'s own stamp file directly, telling the three [`Stamp`]
/// outcomes apart rather than collapsing two of them into `None` the way
/// [`peek`] does.
fn read_stamp(root: &Path) -> Result<Stamp> {
    let Some(common) = common_git_dir(root)? else {
        return Ok(Stamp::None);
    };
    match std::fs::read_to_string(common.join(ID_FILE)) {
        Ok(raw) => {
            let trimmed = raw.trim().to_string();
            if is_valid_id(&trimmed) {
                Ok(Stamp::Valid(trimmed))
            } else {
                Ok(Stamp::Invalid(trimmed))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Stamp::None),
        Err(err) => Err(err).with_context(|| format!("reading {}", common.join(ID_FILE).display())),
    }
}

/// Check `root`'s own stamp against its home's record of it, settling
/// whatever can be settled on its own and refusing what cannot — the seven
/// states the `binding-record` task defines, and the one place a repo-mode
/// home's `project.toml` is ever written. Called from [`Repo::discover`] on
/// every command, not only `init`.
///
/// `root` is already canonicalized, as every caller's is.
///
/// Checked first, ahead of the stamp entirely: a workspace's own
/// `project.toml` is home mode's whole binding, so a checkout it lists is
/// never read from or written to through its `.git` at all — not even the
/// read `read_stamp` below would otherwise do. Only two things follow from
/// that check: the dispatcher folder a workspace already names, or a
/// refusal when the checkout also carries a tracked `.spoolway/` — a claim
/// on the same checkout the stamp-based flow below has no way to arbitrate,
/// so this settles it before that flow ever starts.
///
/// [`workspace_clone_lenient`], not [`workspace_clone_checked`]: this is
/// the one check [`Repo::discover`] runs for *every* ordinary command, so a
/// workspace file broken by a typo on the other side of the machine must
/// never stop it — [`Repo::root`]'s own last-resort scan already refused on
/// `root`'s behalf if that broken file was the one explanation left for not
/// finding it. What this still must not do is resolve `root` itself being
/// listed more than once to whichever entry `read_dir` happened to return
/// first, with nobody told — [`workspace_clone_lenient`] keeps that bail.
/// Every broken file it reports back is noted, once, before this falls
/// through to treating `root` as unlisted.
pub(crate) fn bind(root: &Path) -> Result<PathBuf> {
    let (clone, broken) = workspace_clone_lenient(root)?;
    for record_path in &broken {
        // `bind` runs twice on an ordinary command — once through
        // `gate::notify`'s own lenient discovery, once through the command's
        // own strict one — so printing unconditionally here would say the
        // same broken file twice. `first_time_this_process` is the same
        // process-wide dedup `overrides::print_ignored_notices` already
        // uses for the identical reason. Stderr, not stdout: this is a
        // notice about the machine, not part of a command's own output —
        // `--json` output must still be the one thing on stdout.
        let line = format!(
            "  note  {} does not read as a workspace; skipped",
            record_path.display(),
        );
        if crate::overrides::first_time_this_process(&line) {
            eprintln!("{line}");
        }
    }
    if let Some(clone) = clone {
        let tracked = crate::config::tracked_setup_dir_in(root);
        if tracked.is_dir() {
            bail!(
                "{} has a `.spoolway/` folder and is also listed as a clone in {}\n  \
                 choose one by hand: delete the clone entry from the workspace's project.toml, \
                 or remove `.spoolway/` from the checkout",
                tracked.display(),
                clone.workspace.join(BINDING_FILE).display(),
            );
        }
        return Ok(clone.home_dir());
    }
    match read_stamp(root)? {
        Stamp::Invalid(raw) => {
            // `read_stamp` already found the file, so the common git
            // directory — and therefore this path — resolves; the `expect`
            // only documents that, it never actually has to recover from
            // anything.
            let stamp_path = id_file_path(root)?.expect("a stamp was just read from this path");
            // The second file a malformed id cannot be used to build: an
            // invalid id is exactly what must never reach a path joined
            // onto `state_root()`, so this looks the other way round
            // instead — by `root`, not by `raw` — the same scan
            // [`home_recording`] does for criterion 6. When nothing under
            // `~/.spoolway/` records this checkout by path either, there is
            // no *real* second file to name — but the criterion still
            // wants an absolute path, not only prose, so this names the
            // one place a project.toml for this exact checkout would sit
            // absent any stamp at all: the plain-basename home
            // `crate::mux::project_home` already falls back to whenever
            // nothing else settles it, built from `root`'s own basename,
            // never from the untrusted `raw` id.
            // Whether deleting the stamp alone is enough to let a plain
            // `spoolway init` mint a fresh one: it is, only when nothing
            // under `~/.spoolway/` records this exact path already. When
            // something does, `bind_unstamped`'s own criterion 6 would
            // refuse that rerun — "has no id, but {record} already records
            // this checkout" — rather than mint, since a bare stamp delete
            // leaves that record pointing at an id nothing on disk carries
            // any more. The remedy has to delete that record too, named
            // here so it does not take a second refusal to learn that.
            let (record_line, second_file) = match home_recording(root) {
                Some(home) => {
                    let record_path = home.join(BINDING_FILE);
                    (
                        record_path.display().to_string(),
                        Some(record_path.display().to_string()),
                    )
                }
                None => {
                    let fallback = crate::mux::state_root()
                        .join(crate::mux::project_label(root))
                        .join(BINDING_FILE);
                    (
                        format!(
                            "{} (does not exist — nothing records this checkout)",
                            fallback.display()
                        ),
                        None,
                    )
                }
            };
            let remedy = match &second_file {
                Some(record_path) => format!(
                    "delete {} and {record_path}, then run `spoolway init` again to mint a \
                     fresh one",
                    stamp_path.display(),
                ),
                None => format!(
                    "delete {} and run `spoolway init` again to mint a fresh one",
                    stamp_path.display(),
                ),
            };
            bail!(
                "{} does not hold a usable id: {raw:?} is not six lowercase letters and \
                 digits\n  {}\n  fix it by hand, or {remedy}",
                stamp_path.display(),
                record_line,
            );
        }
        Stamp::Valid(id) => bind_stamped(root, &id),
        Stamp::None => bind_unstamped(root),
    }
}

/// [`bind`], tolerant of its own failure — the one caller allowed to be:
/// [`Repo::discover_lenient`], for the same reason
/// [`crate::mux::project_home_lenient`] exists. Falls back to that same
/// basename-keyed guess for `home` on failure, paired with the real error
/// — never silently, the way an ordinary caller of [`bind`] would be.
fn bind_lenient(root: &Path) -> (PathBuf, Option<anyhow::Error>) {
    match bind(root) {
        Ok(home) => (home, None),
        Err(err) => {
            let (home, _) = crate::mux::project_home_lenient(root);
            (home, Some(err))
        }
    }
}

/// `bind`'s branch for a checkout carrying a valid stamp — acceptance
/// criteria 1 through 4 of `binding-record`.
fn bind_stamped(root: &Path, id: &str) -> Result<PathBuf> {
    let home = crate::mux::project_home(root)?;
    let record_path = home.join(BINDING_FILE);
    // Already known to exist and parse: `read_stamp` just read it.
    let stamp_path = id_file_path(root)?.expect("a valid stamp was just read");

    let binding = match read_binding(&home) {
        Ok(Some(binding)) => binding,
        Ok(None) => {
            // Criterion 4: a valid stamp, but no home records it at all —
            // either the directory itself is gone, or it exists but nobody
            // has ever bound a checkout to it.
            bail!(
                "no home holds the id {id}\n  {}  {id}\n  nothing under {} records it\n  \
                 if a home under {} already holds this project's state under a different \
                 name, edit that home's own project.toml by hand to name this root\n  \
                 otherwise delete {} and run `spoolway init` again to mint a fresh id and a \
                 fresh home",
                stamp_path.display(),
                record_path.display(),
                crate::mux::state_root().display(),
                stamp_path.display(),
            );
        }
        Err(err) => return Err(err),
    };

    if binding.root == root && binding.id == id {
        // Criterion 1: both files already agree. Nothing to do.
        return Ok(home);
    }

    if binding.root == root {
        // The record names this exact checkout, but a different id than
        // the stamp does — the stamp was edited by hand after the record
        // was written, or vice versa. Neither file is more likely right
        // than the other, so this refuses rather than silently trusting
        // one over the other.
        //
        // Deleting the stamp alone and rerunning is not the fix: `home` was
        // computed from `id`, this record's own `root` is already `root`,
        // so `bind_unstamped`'s own `home_recording` scan would find this
        // very `record_path` again and refuse with criterion 6 instead of
        // minting — the remedy has to delete both files.
        bail!(
            "{} and {} disagree about this checkout's id: the stamp says {id}, the record \
             says {}\n  fix one by hand to match the other, or delete both {} and {} and run \
             `spoolway init` again to mint a fresh id both files will agree on",
            stamp_path.display(),
            record_path.display(),
            binding.id,
            stamp_path.display(),
            record_path.display(),
        );
    }

    // The record names a different checkout entirely. Whether that
    // checkout is still the rightful owner turns on whether it still
    // exists *and* still carries this same id — both have to hold for the
    // two to genuinely be in conflict (criterion 3); either one failing
    // means the record is simply stale (criterion 2), and this checkout
    // may take it over rather than being refused over a claim nothing can
    // still make. A real failure reading the other checkout's own stamp —
    // a permissions problem, a corrupt repository — is neither of those:
    // it is refused outright rather than read as "no longer carries the
    // id" and silently taken as licence to transfer the binding.
    let other_stamp = if binding.root.exists() {
        Some(read_stamp(&binding.root))
    } else {
        None
    };
    match other_stamp {
        Some(Ok(Stamp::Valid(other))) if other == id => bail!(
            "two checkouts carry the id {id}\n  {}  recorded in {}, and still carries it\n  \
             {}  this one, stamped at {}\n  delete {} and run `spoolway init` again to \
             re-stamp this one with a fresh id",
            binding.root.display(),
            record_path.display(),
            root.display(),
            stamp_path.display(),
            stamp_path.display(),
        ),
        Some(Err(err)) => {
            return Err(err).with_context(|| {
                format!(
                    "could not tell whether {} still carries the id {id} recorded in {} — \
                     refusing rather than guessing which checkout this binding belongs to. \
                     Fix whatever stopped that checkout's own stamp from being read (often a \
                     permissions problem) and run this again, or delete {} and run `spoolway \
                     init` again in {} to stop depending on the answer at all.",
                    binding.root.display(),
                    record_path.display(),
                    stamp_path.display(),
                    root.display(),
                )
            });
        }
        // The checkout on record is gone, or its own stamp read fine but
        // no longer names this id — either way nothing there can still be
        // telling the truth, so criterion 2 follows below.
        _ => {}
    }

    write_binding(
        &home,
        &Binding {
            id: id.to_string(),
            root: root.to_path_buf(),
        },
    )?;
    println!(
        "spoolway: {} now records {} (was {})",
        record_path.display(),
        root.display(),
        binding.root.display(),
    );
    Ok(home)
}

/// The home under `~/.spoolway/` whose `project.toml` already names `root`
/// as its checkout, if any. The one way an unstamped checkout can be told
/// apart from one nothing has ever recorded at all (criteria 6 and 7):
/// without a stamp there is no id to look a home up by directly, so this
/// scans every home's own record for one instead.
fn home_recording(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(crate::mux::state_root()).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(Some(binding)) = read_binding(&path)
            && binding.root == root
        {
            return Some(path);
        }
    }
    None
}

/// A home-mode workspace's own `project.toml`: the clones that share its
/// `config/`, each found by its path rather than a stamp — see the
/// `home-mode-discovery` task and the plan's `#d-path-binding` drawing.
/// Written by a home-mode `spoolway init`, which creates one with
/// [`create_workspace`] or adds a clone to one with [`join_workspace`], and
/// rewritten in place by a join that takes over a gone checkout's entry,
/// which changes only that clone entry's own `root`. A move ([`move_clone`],
/// reached from `spoolway init`'s own menu through [`move_checkout`]) is the
/// one thing that takes an entry out, and puts it into another workspace.
///
/// Its `clones` field is what tells this shape apart from an ordinary
/// [`Binding`], which this same [`BINDING_FILE`] name holds for a repo-mode
/// home: a `Binding` has a single `root` and no `clones`, so trying to parse
/// one as a `WorkspaceToml` fails on the missing field, and vice versa —
/// `~/.spoolway/` can hold a mix of both kinds of home without either ever
/// being misread as the other. `id` is carried along on both reads and
/// writes even though nothing here ever consults it — the workspace's own
/// id, recorded for a person reading the file by eye (see `#d-path-binding`)
/// — so a takeover's own rewrite never silently drops it.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct WorkspaceToml {
    id: String,
    clones: Vec<CloneEntry>,
}

/// One clone inside a [`WorkspaceToml`]: the checkout's own absolute path,
/// and the name of the `dispatchers/` subdirectory holding its queue,
/// archive and worktrees.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct CloneEntry {
    root: PathBuf,
    dispatcher: String,
    /// `root`'s own [`root_commit`] as of the last time this entry was
    /// written — `None` for an entry a version before this field existed
    /// wrote, for a `root` with no commits yet, or for a shallow clone (see
    /// [`root_commit`]'s own doc comment). The one piece of this entry
    /// [`join_workspace`]'s takeover checks a gone entry against through
    /// [`commit_exists`] rather than only rewrites: `root` itself can be
    /// compared for existence and liveness, but once it is gone there is
    /// nothing left on disk to tell an unrelated repository taking over
    /// this dispatcher by mistake apart from the real clone moved — this is
    /// that fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root_commit: Option<String>,
}

/// The workspace and dispatcher folder a checkout is registered under, if
/// any — home mode's whole discovery. `root` is matched exactly, so a
/// caller holding a worktree path rather than the checkout it belongs to
/// must resolve it to the main checkout first (`Repo::discover` does, by
/// aliasing `checkout` onto `root` the moment this answers `Some` — see its
/// own comment).
#[derive(Debug)]
pub(crate) struct WorkspaceClone {
    pub(crate) workspace: PathBuf,
    pub(crate) dispatcher: String,
}

impl WorkspaceClone {
    /// The workspace's `config/` — home mode's whole tracked setup, what
    /// [`crate::config::setup_dir_in`] answers for every checkout this
    /// names. The one place `workspace.join("config")` is built, so every
    /// caller reads it rather than rejoining it by hand.
    pub(crate) fn config_dir(&self) -> PathBuf {
        self.workspace.join("config")
    }

    /// `dispatchers/<dispatcher>/` — home mode's whole runtime state, what
    /// [`crate::mux::project_home`]/[`Repo::home`] answer for every checkout
    /// this names. The one place `workspace.join("dispatchers").join(…)` is
    /// built, so every caller reads it rather than rejoining it by hand.
    pub(crate) fn home_dir(&self) -> PathBuf {
        self.workspace.join("dispatchers").join(&self.dispatcher)
    }
}

/// [`all_workspaces`]'s own return shape, named only so clippy's
/// `type_complexity` lint stops flagging the function signature.
type Workspaces = (Vec<(PathBuf, WorkspaceToml)>, Vec<PathBuf>);

/// Every workspace's own `project.toml` under `~/.spoolway/`, read once and
/// shared by [`workspace_clone_lenient`] and [`stale_workspace_clones`]
/// rather than each scanning the directory on its own.
///
/// `~/.spoolway/` holds more than workspaces: a 0.6.0 repo-mode home (an
/// ordinary [`Binding`] — `id` and `root`, no `clones`), a legacy home with
/// no `project.toml` at all, and plain folders like `logs/` or
/// `.dispatcher/` that are no home at all. Those are told apart from a
/// workspace by shape, not merely by failing to parse: a folder is a
/// workspace if its `project.toml` carries a `clones` key, or if it sits
/// beside a `config/` or `dispatchers/` folder — what [`create_workspace`]
/// and [`join_workspace`] always write alongside one. Anything else is
/// skipped silently, exactly as before.
///
/// A folder that *does* look like a workspace, but whose `project.toml`
/// cannot be read or parsed, is reported back as `broken` rather than
/// stopping the scan — the `broken-workspace-skipped` task's whole point:
/// this used to `bail!` outright on the first such file, and [`bind`] asks
/// this before it even checks whether the checkout in hand is listed
/// anywhere, so one typo in one workspace's `project.toml` stopped every
/// command on the machine, repo-mode projects included, with an error
/// naming a file that had nothing to do with them. A caller that finds no
/// match for the checkout it cares about decides for itself whether a
/// broken file nearby is reason enough to refuse; one that finds its match
/// anyway, or was never looking for one in the first place, is never
/// stopped by it.
///
/// A `dispatcher` field that is not one plain name is a different kind of
/// problem — the file read and parsed fine, it just holds a value nothing
/// should ever trust (see the bail below) — so that still refuses outright
/// rather than joining `broken`.
fn all_workspaces() -> Result<Workspaces> {
    let Ok(entries) = std::fs::read_dir(crate::mux::state_root()) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut found = Vec::new();
    let mut broken = Vec::new();
    for path in entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
    {
        let record_path = path.join(BINDING_FILE);
        let looks_like_workspace =
            path.join("config").is_dir() || path.join("dispatchers").is_dir();
        let raw = match std::fs::read_to_string(&record_path) {
            Ok(raw) => raw,
            Err(_) if looks_like_workspace => {
                broken.push(record_path);
                continue;
            }
            Err(_) => continue,
        };
        let has_clones_key = raw
            .parse::<toml::Value>()
            .ok()
            .and_then(|value| value.get("clones").cloned())
            .is_some();
        match toml::from_str::<WorkspaceToml>(&raw) {
            Ok(workspace) => {
                // A `dispatcher` field is joined straight onto the
                // workspace folder by `WorkspaceClone::home_dir` — never
                // checked when `spoolway init` writes it, because
                // every writer already mints or validates it, but nothing
                // has ever stopped a hand edit from putting a path
                // separator or a `..` there instead. Checked here, once,
                // where every reader of the file would otherwise trust it.
                if let Some(bad) = workspace
                    .clones
                    .iter()
                    .find(|clone| !crate::tracking::is_bare_filename(&clone.dispatcher))
                {
                    bail!(
                        "{} names dispatcher {:?} for {} — a dispatcher must be one plain name, \
                         with no path separator and no `..`, since it is joined straight onto \
                         the workspace's own folder\n  fix it by hand, keeping whichever of \
                         `dispatchers/*` actually holds that clone's queue and worktrees",
                        record_path.display(),
                        bad.dispatcher,
                        bad.root.display(),
                    );
                }
                found.push((path, workspace));
            }
            Err(_) if looks_like_workspace || has_clones_key => broken.push(record_path),
            Err(_) => continue,
        }
    }
    Ok((found, broken))
}

/// [`WorkspaceClone`] for `root`, if some *readable* workspace's
/// `project.toml` lists it, paired with the record path of every workspace
/// file [`all_workspaces`] could not read or parse — never a hard failure
/// on its own, since a checkout settled some other way (its own tracked
/// `.spoolway/`, or a match found here regardless) has no reason to care
/// that an unrelated workspace file is broken. Still a hard failure when
/// `root` itself is listed more than once among the readable workspaces —
/// in one workspace's `clones`, or across several. That case is collected
/// across every match rather than stopping at the first, exactly because
/// the bug it guards against is picking one of several entries at random:
/// `read_dir`'s order is unspecified, so a silent `find_map` would bind to
/// whichever the filesystem happened to return first, differently from one
/// run to the next.
///
/// [`bind`] and `doctor`'s own `registration_check` are the two callers
/// that need exactly this: a match if there is one, the broken list to note
/// and move past, and still a hard refusal on a genuine duplicate. Neither
/// is answering the one question [`workspace_clone_checked`] exists for —
/// whether `root` itself might be the checkout a broken file would have
/// named — because both already know better by the time they ask:
/// [`Repo::root`] settled that question before either of them runs.
pub(crate) fn workspace_clone_lenient(
    root: &Path,
) -> Result<(Option<WorkspaceClone>, Vec<PathBuf>)> {
    let (workspaces, broken) = all_workspaces()?;
    let mut matches: Vec<(PathBuf, WorkspaceClone)> = Vec::new();
    for (workspace, toml) in &workspaces {
        for clone in &toml.clones {
            if clone.root == root {
                matches.push((
                    workspace.join(BINDING_FILE),
                    WorkspaceClone {
                        workspace: workspace.clone(),
                        dispatcher: clone.dispatcher.clone(),
                    },
                ));
            }
        }
    }
    if matches.len() > 1 {
        bail!(
            "{} is listed more than once:\n{}\n  only one entry may name a checkout — remove \
             every other one by hand, keeping whichever dispatcher folder is still in use",
            root.display(),
            matches
                .iter()
                .map(|(file, clone)| format!(
                    "  {} — dispatcher {}",
                    file.display(),
                    clone.dispatcher
                ))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Ok((matches.into_iter().next().map(|(_, clone)| clone), broken))
}

/// [`workspace_clone_lenient`], refusing outright when `root` matches no
/// readable workspace *and* some workspace file nearby could not be read —
/// `root` might be exactly the checkout that file would have named, so
/// answering "not listed" here would be a guess, not a fact. Used where
/// that guess is exactly the bug being fixed: [`Repo::root`]'s own
/// last-resort scan, before it falls through to "no spoolway project found
/// … run `spoolway init`", and `init`'s own `Placement::choose`, before it
/// falls through to converting the clone it could not read about to repo
/// mode instead.
pub(crate) fn workspace_clone_checked(root: &Path) -> Result<Option<WorkspaceClone>> {
    let (found, broken) = workspace_clone_lenient(root)?;
    if found.is_none()
        && let Some(first) = broken.first()
    {
        bail!(
            "this checkout is in no workspace spoolway can read, and {} does not parse.",
            first.display(),
        );
    }
    Ok(found)
}

/// [`workspace_clone_checked`], with a broken or duplicate workspace entry
/// elsewhere on the machine treated the same as not finding `root` at all —
/// the one call every setup and home accessor goes through to notice home
/// mode at all: [`crate::config::setup_dir_in`] for the tracked setup, and
/// [`crate::mux::project_home`] for the dispatcher folder, both of which run
/// on every command and have no way to surface an error about a workspace
/// that is not even the one in play. Neither has to: [`Repo::root`]'s own
/// last-resort scan, ahead of either on every ordinary command through
/// [`Repo::discover`], already refused on `root`'s behalf if a broken file
/// was the one explanation left for not finding it — by the time this runs,
/// a broken file nearby is never a reason to doubt the answer.
pub(crate) fn workspace_clone(root: &Path) -> Option<WorkspaceClone> {
    workspace_clone_checked(root).unwrap_or(None)
}

/// Every checkout `workspace`'s own `project.toml` lists, other than
/// `except` — the clones that share its `config/`, so `init --force` can
/// name them before rewriting a file every one of them reads. Best-effort
/// like [`workspace_clone`]: a workspace file that cannot be read or parsed
/// answers no siblings rather than turning a warning into a reason `init`
/// itself fails.
pub(crate) fn sibling_clones(workspace: &Path, except: &Path) -> Vec<PathBuf> {
    let record_path = workspace.join(BINDING_FILE);
    let Ok(raw) = std::fs::read_to_string(&record_path) else {
        return Vec::new();
    };
    let Ok(toml) = toml::from_str::<WorkspaceToml>(&raw) else {
        return Vec::new();
    };
    toml.clones
        .into_iter()
        .map(|clone| clone.root)
        .filter(|clone_root| clone_root != except)
        .collect()
}

/// A `spoolway init --workspace <name>` line for every workspace that
/// [`start`] would actually take a gone entry's queue over in — the hint
/// [`Repo::root`]'s own "no spoolway project found" appends when it has
/// one, for a checkout that moved or was deleted without being re-attached.
/// Mirrors [`join_workspace`]'s own takeover rule exactly, rather than
/// listing every gone entry on the machine regardless of whether running
/// the line would actually take it over: a workspace prints a line only
/// when exactly one of its entries is both gone and a match for `start`'s
/// own root commit, through [`commit_exists`] — the same test
/// [`join_workspace`] runs. A workspace with a gone entry from an unrelated
/// repository, with two or more gone entries of this one, or whose entry
/// recorded no root commit at all, prints nothing: naming the command
/// there would promise a takeover `join_workspace` would not actually make,
/// joining `start` as a new clone instead with its old queue left behind.
/// This is the only place any such entry is reported; nothing here removes
/// one.
///
/// `start` carrying no root commit of its own — no commits yet, or a
/// shallow clone — can never match anything, the same rule
/// [`join_workspace`] applies, so this returns no lines at all rather than
/// asking `all_workspaces` for nothing.
///
/// The `<name>` argument is shell-quoted — a workspace folder a person
/// named by hand can carry a space or another character a shell would
/// otherwise split on — and each line names the matched entry's own old
/// `root`, so a person staring at several stale lines at once can tell
/// which checkout each one is actually offering to take over.
///
/// Best-effort like [`workspace_clone`]: by the time this runs,
/// [`Repo::root`] has already let a broken workspace file's own error
/// through if there was one to report, so a failure here is some other
/// workspace's, worth degrading to "no hint" rather than replacing the
/// not-found error this only ever appends to.
fn stale_workspace_clones(start: &Path) -> Vec<String> {
    let Some(mine) = root_commit(start) else {
        return Vec::new();
    };
    all_workspaces()
        .unwrap_or_default()
        .0
        .into_iter()
        .filter_map(|(workspace, toml)| {
            let name = workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| workspace.display().to_string());
            let mut matching = toml.clones.into_iter().filter(|clone| {
                !clone.root.exists()
                    && clone
                        .root_commit
                        .as_deref()
                        .is_some_and(|commit| commit == mine || commit_exists(start, commit))
            });
            match (matching.next(), matching.next()) {
                (Some(entry), None) => Some(format!(
                    "spoolway init --workspace {} — was {}",
                    crate::platform::quote(&name),
                    entry.root.display(),
                )),
                _ => None,
            }
        })
        .collect()
}

/// `bind`'s branch for a checkout carrying no stamp at all — acceptance
/// criteria 6 and 7 of `binding-record`.
fn bind_unstamped(root: &Path) -> Result<PathBuf> {
    if let Some(home) = home_recording(root) {
        // Criterion 6: some home already names this exact checkout, but the
        // checkout itself carries no id to confirm it with — the stamp was
        // deleted or never made it into this clone. Refused rather than
        // silently re-stamped: writing a fresh id here would leave that
        // home's record pointing at an id nothing on disk carries any more.
        bail!(
            "{} has no id, but {} already records this checkout\n  \
             restore the stamp from the id in that file by hand, or delete the project.toml \
             entry and run `spoolway init` again to mint this checkout a fresh id",
            common_git_dir(root)?
                .map(|dir| dir.join(ID_FILE).display().to_string())
                .unwrap_or_else(|| root.display().to_string()),
            home.join(BINDING_FILE).display(),
        );
    }

    // Criterion 7: nothing records this checkout anywhere, and it carries
    // no stamp of its own — there is nothing to guess, so it binds itself,
    // the same mint `spoolway init` has always done, just no longer gated
    // on someone having run `init` first.
    let Some((id, _minted)) = stamped_id(root)? else {
        bail!(
            "{} has no git repository behind it — spoolway keys a project's home off an id \
             stamped into its own `.git`, so there is nowhere to write one. Run `git init` \
             here first.",
            root.display()
        );
    };
    let home = crate::mux::project_home(root)?;
    write_binding(
        &home,
        &Binding {
            id,
            root: root.to_path_buf(),
        },
    )?;
    Ok(home)
}

/// Whether a worktree cut under `home` is genuinely in use right now — the
/// general signal [`move_clone`] refuses on, covering every
/// backend a lane can run under rather than one. A headless lane is not
/// the only way a worktree ends up live: the default herdr backend keeps a
/// pane's own shell running in one, and a `commands:` pipeline step spawns
/// a process there too, and neither writes the pid-per-lane record
/// [`crate::headless::lane_working_under`] alone reads. [`process_cwd_under`]
/// is the one check that answers for all three at once, on every platform —
/// any process with its own current directory somewhere under
/// `home/worktrees` is a live worktree by definition, whoever started it —
/// with the headless check kept alongside it regardless, since a lane
/// record answers even for a backend this build has no other way to ask
/// about.
///
/// Scoped to `home/worktrees` specifically, not `home` as a whole: a shell
/// merely `cd`'d into `queue/`, `archive/` or the home's own root is not a
/// worktree at all, and refusing a safe migration over it would be exactly
/// the over-broad refusal acceptance criterion 2 does not ask for.
fn any_worktree_in_use(home: &Path) -> bool {
    let worktrees = home.join("worktrees");
    process_cwd_under(&worktrees)
        || crate::headless::lane_working_under(&home.join(crate::headless::LANE_DIR), &worktrees)
}

/// Whether any currently running process has its own working directory
/// somewhere under `dir` — checked through `/proc`, which is what makes
/// this backend-agnostic: a process's cwd is set once, by whatever started
/// it, and the kernel keeps `/proc/<pid>/cwd` resolved to wherever that
/// directory is *now*, even after something else renamed it out from under
/// the process — exactly what [`move_clone`]'s own move does to a worktree
/// a lane is sitting in. `is_running` is checked too, not just that the
/// symlink resolves: `/proc/<pid>` briefly outlives a process that has
/// already exited on some kernels, and a directory this reads as "in use"
/// must mean a process that still is.
#[cfg(target_os = "linux")]
fn process_cwd_under(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd"))
            && cwd.starts_with(dir)
            && crate::lock::is_running(pid)
        {
            return true;
        }
    }
    false
}

/// The same question as the Linux arm above, answered through `sysinfo`
/// instead of `/proc`: macOS has no file for this to read directly — it
/// takes a libproc call instead — and `sysinfo` already carries that behind
/// one call, refreshed for cwd alone rather than every metric it can report.
/// A process caught mid-exit is not a concern here
/// the way it is for the Linux `is_running` check: `sysinfo` only lists
/// processes it could actually query just now, so a stale entry for one
/// already gone does not linger the way a `/proc/<pid>` directory briefly
/// can.
#[cfg(not(target_os = "linux"))]
fn process_cwd_under(dir: &Path) -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .any(|process| process.cwd().is_some_and(|cwd| cwd.starts_with(dir)))
}

/// Overwrite `root`'s own stamp with `id`, whatever it already held —
/// [`read_or_mint`]'s idempotent read is exactly what this must not get,
/// bypassing it to set up the two-checkouts-sharing-an-id fixtures
/// [`bind`]'s own tests need, which no ordinary path ever writes on
/// purpose any more.
///
/// A label is minted the same way [`stamped_id`] would for a checkout
/// stamped for the very first time, when `root` does not already carry
/// one — needed for [`crate::mux::project_home`] to key off afterwards.
#[cfg(test)]
fn stamp_over(root: &Path, id: &str) -> Result<()> {
    let common = common_git_dir(root)?.with_context(|| {
        format!(
            "{} has no git repository behind it — spoolway keys a project's home off an id \
             stamped into its own `.git`.",
            root.display()
        )
    })?;
    crate::task::write_atomic(&common.join(ID_FILE), id)?;
    if peek(&common.join(LABEL_FILE), is_valid_label)?.is_none() {
        crate::task::write_atomic(
            &common.join(LABEL_FILE),
            sanitize_label(&crate::mux::project_label(root)),
        )?;
    }
    Ok(())
}

/// The advisory lock [`crate::lock::WorkspaceLock`] takes over `workspace`'s
/// `project.toml` read-modify-write — see that type's doc. Named off
/// `BINDING_FILE` itself and left inside `workspace`, not a folder of its
/// own, so it adds nothing for [`all_workspaces`] or any other walk of
/// `~/.spoolway/` to trip over.
fn workspace_lock_path(workspace: &Path) -> PathBuf {
    workspace.join(format!("{BINDING_FILE}.lock"))
}

/// Write `workspace`'s `project.toml` whole, in the same explained-header
/// style [`write_binding`] uses for a repo-mode home's own record. The one
/// place a workspace's own file is ever written by this binary: by `init`,
/// through [`create_workspace`], [`join_workspace`] and [`move_checkout`],
/// and by [`move_clone`]. Only a move takes an entry out; a stale one
/// nothing has taken over stays.
fn write_workspace(workspace: &Path, toml_value: &WorkspaceToml) -> Result<()> {
    let body = format!(
        "# The clones that read this workspace's config/, each found by its path.\n\
         # Nothing is stamped into any clone. `spoolway init` adds a clone here,\n\
         # and takes a gone clone's entry over when its root commit matches.\n\
         # root_commit is that clone's first commit, checked against a\n\
         # checkout taking a gone entry over so it refuses an unrelated\n\
         # repository — delete the line to skip that check for one entry.\n{}",
        toml::to_string_pretty(toml_value).context("serialising project.toml")?
    );
    let path = workspace.join(BINDING_FILE);
    crate::task::write_atomic(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// One workspace as `init`'s workspace menu lists it: its folder name under
/// `~/.spoolway/`, the repository its clones belong to, and every root commit
/// its clone entries record.
pub(crate) struct WorkspaceSummary {
    pub(crate) name: String,
    /// The repository this workspace's clones belong to, as the menu row
    /// shows it for a workspace of another repository: the first listed
    /// clone's `origin` URL, or its path shortened under `$HOME` when it has
    /// none. `None` when the workspace lists no clones at all.
    pub(crate) repo_display: Option<String>,
    /// Every distinct root commit across the workspace's clone entries:
    /// each entry's recorded `root_commit`, or, for an entry written before
    /// that field existed, the commit read off its folder while it still
    /// exists. Every clone counts, not only the first: the first entry is
    /// often a checkout long since deleted, and a workspace whose first
    /// entry recorded nothing used to read as another repository even with
    /// a clone of this one listed right after it.
    root_commits: Vec<String>,
}

impl WorkspaceSummary {
    /// Whether this workspace holds a clone of the repository `root` is
    /// a checkout of, `mine` being `root`'s own [`root_commit`]. Same
    /// repository means the same root commit, nothing weaker: an `origin`
    /// URL is spelled differently over SSH and HTTPS and is missing on a
    /// local clone, so it was dropped as a signal. A recorded commit is also
    /// accepted when `root` merely holds it, through [`commit_exists`],
    /// because a repository with several root commits names whichever one
    /// its current branch reaches first. A checkout with no root commit — no
    /// commits yet, or a shallow clone — matches nothing.
    pub(crate) fn holds_repository_of(&self, root: &Path, mine: Option<&str>) -> bool {
        let Some(mine) = mine else { return false };
        self.root_commits
            .iter()
            .any(|commit| commit == mine || commit_exists(root, commit))
    }

    /// Whether `root` may move into this workspace: one holding its own
    /// repository, never another's. A checkout with no root commit has
    /// nothing to compare, so it may move only into a workspace that has no
    /// root commit recorded either — anything else could be an unrelated
    /// repository, and a move carries the checkout's queue with it.
    pub(crate) fn may_move_into(&self, root: &Path, mine: Option<&str>) -> bool {
        match mine {
            Some(_) => self.holds_repository_of(root, mine),
            None => self.root_commits.is_empty(),
        }
    }
}

/// Every workspace under `~/.spoolway/`, sorted by name so the menu reads
/// the same on every run — [`all_workspaces`] follows `read_dir`'s order,
/// which is whatever the filesystem happens to hand back.
///
/// Lenient like [`workspace_clone`]: this only ever runs after
/// `Placement::choose`'s own fallible scan already found nothing broken, so
/// a failure here would be a race with something else on the machine, not
/// news — degrading to "no workspaces" is no worse than the menu this feeds
/// already being empty.
pub(crate) fn workspaces() -> Vec<WorkspaceSummary> {
    let mut found: Vec<WorkspaceSummary> = all_workspaces()
        .unwrap_or_default()
        .0
        .into_iter()
        .filter_map(|(path, toml)| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let repo_display = toml
                .clones
                .first()
                .map(|clone| match origin_url(&clone.root) {
                    Some(url) => url,
                    None => shorten_home(&clone.root),
                });
            let mut root_commits: Vec<String> = Vec::new();
            for clone in &toml.clones {
                let commit = clone.root_commit.clone().or_else(|| {
                    clone
                        .root
                        .exists()
                        .then(|| root_commit(&clone.root))
                        .flatten()
                });
                if let Some(commit) = commit
                    && !root_commits.contains(&commit)
                {
                    root_commits.push(commit);
                }
            }
            Some(WorkspaceSummary {
                name,
                repo_display,
                root_commits,
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// One workspace as `config path --json`'s own `workspaces` list carries it
/// — see the `config-path-places` task. Unlike [`WorkspaceSummary`], which
/// is `init`'s menu and only ever shows a workspace whose `project.toml`
/// parsed, this is the skill's whole map of `~/.spoolway/`, so a workspace
/// whose file could not be read still gets a row, with `error` naming why
/// rather than the row — or the rest of the command — disappearing.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct WorkspacePlace {
    pub(crate) name: String,
    pub(crate) config: PathBuf,
    pub(crate) clones: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Every workspace under `~/.spoolway/`, readable or not, sorted by name —
/// [`workspaces`]'s own `project.toml`-must-parse twin for `config path`.
/// Lenient the same way: a workspace folder [`all_workspaces`] itself could
/// not even list (its `dispatcher` field holding a path instead of a plain
/// name, say) degrades to an empty list rather than failing the whole
/// command, since that is a different, rarer corruption than the one
/// `error` here exists to report.
pub(crate) fn workspace_places() -> Vec<WorkspacePlace> {
    let (found, broken) = all_workspaces().unwrap_or_default();
    let mut places: Vec<WorkspacePlace> = found
        .into_iter()
        .filter_map(|(path, toml)| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            Some(WorkspacePlace {
                config: path.join("config"),
                clones: toml.clones.len(),
                name,
                error: None,
            })
        })
        .collect();
    for record_path in broken {
        let Some(dir) = record_path.parent() else {
            continue;
        };
        let Some(name) = dir.file_name() else {
            continue;
        };
        places.push(WorkspacePlace {
            name: name.to_string_lossy().into_owned(),
            config: dir.join("config"),
            clones: 0,
            error: Some(format!(
                "{} does not read as a workspace's project.toml",
                record_path.display()
            )),
        });
    }
    places.sort_by(|a, b| a.name.cmp(&b.name));
    places
}

/// Start a new workspace for `root`: `~/.spoolway/<label>-<id>/` holding an
/// empty `config/`, `dispatchers/<name>/` for this clone, and a
/// `project.toml` listing it. `<label>` is `root`'s basename and `<id>` a
/// fresh one, the same shape a repo-mode home takes, so the two kinds of
/// folder sit side by side under `~/.spoolway/` without either needing a
/// prefix to tell them apart.
///
/// Nothing is written into `root` or its `.git`: the workspace's own
/// `project.toml` is the whole of the binding.
pub(crate) fn create_workspace(root: &Path) -> Result<WorkspaceClone> {
    require_utf8_root(root)?;
    require_git_repository(root)?;
    let (workspace, id, label) = new_workspace_folder(root)?;
    let clone = WorkspaceClone {
        workspace: workspace.clone(),
        dispatcher: label,
    };
    let home = clone.home_dir();
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    // Written last, so a workspace is only ever found — by `all_workspaces`,
    // which reads nothing but this file — once its folders are all there.
    write_workspace(
        &workspace,
        &WorkspaceToml {
            id,
            clones: vec![CloneEntry {
                root: root.to_path_buf(),
                dispatcher: clone.dispatcher.clone(),
                root_commit: root_commit(root),
            }],
        },
    )?;
    Ok(clone)
}

/// A fresh `~/.spoolway/<label>-<id>/` with an empty `config/` and
/// `dispatchers/`, and no `project.toml` yet — the part of
/// [`create_workspace`] that [`move_checkout`] shares when a checkout moves
/// into a new workspace. Answers the folder, its id and `root`'s label.
fn new_workspace_folder(root: &Path) -> Result<(PathBuf, String, String)> {
    let state = crate::mux::state_root();
    std::fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    let label = sanitize_label(&crate::mux::project_label(root));
    // `create_dir`, not `create_dir_all`: it fails when the folder is
    // already there, so a fresh id that happens to name an existing folder —
    // or a second `init` racing this one to the same name — draws again
    // rather than both writing into one folder.
    let (workspace, id) = loop {
        let id = generate_id();
        let candidate = state.join(format!("{label}-{id}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break (candidate, id),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    };
    for dir in [workspace.join("config"), workspace.join("dispatchers")] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok((workspace, id, label))
}

/// Add `root` to the workspace named `name` under `~/.spoolway/`, with a
/// dispatcher folder of its own, and leave its `config/` exactly as it is —
/// every clone of a workspace reads the one setup.
///
/// The dispatcher folder takes `root`'s basename, with `-2`, `-3` and so on
/// added while that name is taken — by another clone entry, or by a folder
/// already under `dispatchers/` that a clone entry no longer names, whose
/// queue and worktrees must not be picked up by a clone they never belonged
/// to. A checkout the workspace already lists keeps its own entry.
///
/// One case takes over an entry instead of adding one: exactly one entry of
/// this repository, by root commit, whose folder no longer exists. Its
/// `root` is rewritten to this checkout, and this checkout carries on with
/// its queue, archive and worktrees — see the comment where it happens.
pub(crate) fn join_workspace(root: &Path, name: &str) -> Result<WorkspaceClone> {
    require_utf8_root(root)?;
    require_git_repository(root)?;
    if !crate::tracking::is_bare_filename(name) {
        bail!("`{name}` is not a workspace name — it cannot carry a path separator or a `..`");
    }
    let workspace = crate::mux::state_root().join(name);
    let record_path = workspace.join(BINDING_FILE);
    // Checked before `WorkspaceLock::acquire`, which does not create the
    // lock file's parent — `workspace` itself — and so needs it there
    // already. Checking here also turns a typo'd `--workspace` name into a
    // plain "no workspace named …" rather than a failed lock write.
    if !record_path.is_file() {
        bail!(
            "no workspace named {name} exists under {}",
            crate::mux::state_root().display()
        );
    }
    // Held across the whole read-modify-write below, so a second join
    // racing this one on the same workspace waits rather than reading the
    // same `parsed.clones` this call is about to write back on top of —
    // see `WorkspaceLock`'s own doc for the lost-entry bug this closes.
    let _lock = crate::lock::WorkspaceLock::acquire(&workspace_lock_path(&workspace))?;
    let raw = std::fs::read_to_string(&record_path).with_context(|| {
        format!(
            "no workspace named {name} exists under {}",
            crate::mux::state_root().display()
        )
    })?;
    let mut parsed: WorkspaceToml = toml::from_str(&raw).with_context(|| {
        format!(
            "{} does not read as a workspace's project.toml",
            record_path.display()
        )
    })?;
    if let Some(existing) = parsed.clones.iter().find(|clone| clone.root == root) {
        return Ok(WorkspaceClone {
            workspace,
            dispatcher: existing.dispatcher.clone(),
        });
    }
    // `create_workspace` writes `config/` before `project.toml`, so a
    // workspace without one has lost it since — deleted by hand, or left by
    // an older failed run. Joining it would hand this clone a dispatcher
    // folder that reads no setup at all.
    let config = workspace.join("config");
    if !config.is_dir() {
        bail!(
            "workspace {name} has no setup: {} is missing — restore it, or start a new \
             workspace with `spoolway init --workspace new`",
            config.display()
        );
    }
    // A checkout of this repository whose folder is gone left its queue
    // behind under its own dispatcher folder. Exactly one such entry is
    // taken over without asking: this checkout is that clone moved or
    // re-cloned, and its tasks carry on. None means an ordinary join, and
    // two or more means there is no telling which one this checkout
    // replaces, so it joins as new rather than guessing. The root commit is
    // what makes it the same repository; a checkout with none — no commits
    // yet, or a shallow clone — never takes over, and neither does an entry
    // that recorded none.
    let mine = root_commit(root);
    if let Some(mine) = &mine {
        let mut gone = parsed.clones.iter_mut().filter(|clone| {
            !clone.root.exists()
                && clone
                    .root_commit
                    .as_deref()
                    .is_some_and(|commit| commit == mine || commit_exists(root, commit))
        });
        if let (Some(entry), None) = (gone.next(), gone.next()) {
            entry.root = root.to_path_buf();
            entry.root_commit = Some(mine.clone());
            let clone = WorkspaceClone {
                workspace: workspace.clone(),
                dispatcher: entry.dispatcher.clone(),
            };
            write_workspace(&workspace, &parsed)?;
            return Ok(clone);
        }
    }
    let base = sanitize_label(&crate::mux::project_label(root));
    let taken = |candidate: &str| {
        parsed
            .clones
            .iter()
            .any(|clone| clone.dispatcher == candidate)
            || (WorkspaceClone {
                workspace: workspace.clone(),
                dispatcher: candidate.to_string(),
            })
            .home_dir()
            .exists()
    };
    let dispatcher = std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded run of names always has a free one");
    let clone = WorkspaceClone {
        workspace: workspace.clone(),
        dispatcher: dispatcher.clone(),
    };
    let home = clone.home_dir();
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    parsed.clones.push(CloneEntry {
        root: root.to_path_buf(),
        dispatcher,
        root_commit: mine,
    });
    // A failure here has already created `home` above — remove it rather
    // than leave a dispatcher folder no entry in `project.toml` ever claims,
    // which would otherwise just draw `<dispatcher>-2` on the retry.
    if let Err(err) = write_workspace(&workspace, &parsed) {
        let _ = std::fs::remove_dir_all(&home);
        return Err(err);
    }
    Ok(clone)
}

/// The ids of every task queued in `clone`'s dispatcher folder that holds a
/// worktree, sorted — what refuses a move through `spoolway init`.
///
/// A task holding a worktree records its path in `worktree_path`, and that
/// path sits under the dispatcher folder a move renames. Left as it is, the
/// task would run as borrowed from a path that no longer exists, and its
/// worktree and branch would be left behind; rewritten, it would still point
/// git at a worktree the dispatcher did not cut there. So the move waits
/// until the task is finished or unqueued instead. A queue file that does not
/// parse counts when its text carries a `worktree_path:` line, since a
/// refusal is cheaper than a stranded worktree.
pub(crate) fn tasks_holding_worktrees(clone: &WorkspaceClone) -> Result<Vec<String>> {
    let (tasks, problems) =
        crate::task::load_dir(&clone.home_dir().join(crate::config::QUEUE_DIR))?;
    let mut held: Vec<String> = tasks
        .iter()
        .filter(|task| task.front.worktree_path.is_some())
        .map(|task| task.id().to_string())
        .collect();
    for problem in problems {
        let raw = std::fs::read_to_string(&problem.path).unwrap_or_default();
        if raw.lines().any(|line| line.starts_with("worktree_path:"))
            && let Some(stem) = problem.path.file_stem()
        {
            held.push(stem.to_string_lossy().into_owned());
        }
    }
    held.sort();
    Ok(held)
}

/// What [`move_checkout`] did: where the checkout is listed now, and the
/// folder name of the workspace the move emptied and removed, if it did.
pub(crate) struct Moved {
    pub(crate) clone: WorkspaceClone,
    pub(crate) removed: Option<String>,
}

/// Move `root` from the workspace that lists it into the workspace named
/// `to`, or into a new workspace when `to` is `None` — what picking another
/// workspace in `spoolway init`'s menu does.
///
/// Refused, with nothing written, in three cases. A task in this checkout's
/// queue holds a worktree (see [`tasks_holding_worktrees`]), and the refusal
/// names each one. `to` holds another repository (see
/// [`WorkspaceSummary::may_move_into`]), and nothing forces that. `to` has
/// lost its `config/`, which a moved checkout would then read no setup from.
///
/// The dispatcher folder keeps its name at `to` when it is free there, and
/// otherwise takes the first free `-2`, `-3` and so on: with no task holding
/// a worktree, nothing records the folder's path, so renaming it is safe.
/// The live-work checks and the folder rename are [`move_clone`]'s.
///
/// The workspace the move leaves with no clone listed is removed, with its
/// `config/` and its entry in the usage registry's `projects.json`: nothing
/// can reach that setup any more, and it would otherwise sit in every later
/// workspace menu as a workspace with no checkouts.
pub(crate) fn move_checkout(root: &Path, to: Option<&str>) -> Result<Moved> {
    let from = check_move(root, to)?;
    let (to_name, fresh) = match to {
        Some(to) => (to.to_string(), None),
        None => {
            let (workspace, id, _label) = new_workspace_folder(root)?;
            write_workspace(
                &workspace,
                &WorkspaceToml {
                    id,
                    clones: Vec::new(),
                },
            )?;
            let name = workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (name, Some(workspace))
        }
    };

    let to_workspace = crate::mux::state_root().join(&to_name);
    let listed: Vec<String> = std::fs::read_to_string(to_workspace.join(BINDING_FILE))
        .ok()
        .and_then(|raw| toml::from_str::<WorkspaceToml>(&raw).ok())
        .map(|toml| {
            toml.clones
                .into_iter()
                .map(|clone| clone.dispatcher)
                .collect()
        })
        .unwrap_or_default();
    let free = |candidate: &str| {
        !listed.iter().any(|name| name == candidate)
            && !to_workspace.join("dispatchers").join(candidate).exists()
    };
    let base = from.dispatcher.clone();
    let dispatcher = std::iter::once(base.clone())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|candidate| free(candidate))
        .expect("an unbounded run of names always has a free one");

    let clone = match move_clone(root, &to_name, Some(&dispatcher)) {
        Ok(clone) => clone,
        Err(err) => {
            // A new workspace made only to receive this checkout is taken
            // back out, so a refused move leaves no empty workspace behind.
            if let Some(workspace) = fresh {
                let _ = std::fs::remove_dir_all(workspace);
            }
            return Err(err);
        }
    };
    let removed = remove_if_empty(&from.workspace)?;
    Ok(Moved { clone, removed })
}

/// Every refusal [`move_checkout`] makes before it writes anything, and the
/// workspace `root` moves out of. `spoolway init` calls this right after its
/// menu, before asking anything else, so a refused move costs nothing; the
/// move calls it again, since time has passed in between.
pub(crate) fn check_move(root: &Path, to: Option<&str>) -> Result<WorkspaceClone> {
    require_utf8_root(root)?;
    let from = workspace_clone_checked(root)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not in any workspace, so there is nothing to move",
            root.display()
        )
    })?;
    let held = tasks_holding_worktrees(&from)?;
    if !held.is_empty() {
        let (verb, noun, them) = if held.len() == 1 {
            ("holds", "a worktree", "it")
        } else {
            ("hold", "worktrees", "them")
        };
        bail!(
            "{} {verb} {noun} in this checkout.\n  Finish or unqueue {them}, then run `spoolway \
             init` again.",
            and_list(&held),
        );
    }
    let Some(to) = to else { return Ok(from) };
    let Some(target) = workspaces().into_iter().find(|w| w.name == to) else {
        bail!(
            "no workspace named {to} exists under {}.\n  Run `spoolway init` and pick one from \
             the menu.",
            shorten_home(&crate::mux::state_root())
        );
    };
    if !target.may_move_into(root, root_commit(root).as_deref()) {
        bail!(
            "workspace {to} holds another repository, so this checkout cannot move there.\n  \
             Run `spoolway init` and pick a workspace of this repository, or create a new one."
        );
    }
    let config = crate::mux::state_root().join(to).join("config");
    if !config.is_dir() {
        bail!(
            "workspace {to} has no setup: {} is missing.\n  Restore it, or run `spoolway init` \
             and pick another workspace.",
            config.display()
        );
    }
    Ok(from)
}

/// `items` as a sentence names them: `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Remove `workspace` when its `project.toml` lists no clone, together with
/// every usage-registry entry whose home sits inside it, and answer its
/// folder name. The clone list is read under the workspace's own lock, so a
/// join that landed first keeps the workspace. The lock is let go before the
/// folder goes, because it lives inside it and Windows refuses to delete an
/// open file.
fn remove_if_empty(workspace: &Path) -> Result<Option<String>> {
    let empty = {
        let _lock = crate::lock::WorkspaceLock::acquire(&workspace_lock_path(workspace))?;
        let record = workspace.join(BINDING_FILE);
        let raw = std::fs::read_to_string(&record)
            .with_context(|| format!("reading {}", record.display()))?;
        let parsed: WorkspaceToml = toml::from_str(&raw).with_context(|| {
            format!(
                "{} does not read as a workspace's project.toml",
                record.display()
            )
        })?;
        parsed.clones.is_empty()
    };
    if !empty {
        return Ok(None);
    }
    std::fs::remove_dir_all(workspace)
        .with_context(|| format!("removing {}", workspace.display()))?;
    crate::usage::registry::forget_under(workspace);
    Ok(workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned()))
}

/// Move `root`'s home-mode registration from the workspace it is listed in
/// now to `to`, carrying its dispatcher folder — queue, archive and
/// worktrees — along rather than leaving it behind. Reached from
/// `spoolway init`'s own menu, through [`move_checkout`].
///
/// The folder keeps its current name at `to` unless that name is already
/// taken there, in which case this refuses rather than silently drawing
/// `-2` the way [`join_workspace`] does for a fresh clone: a move carries
/// live work across workspaces, and renaming its folder out from under it
/// without being asked is exactly the kind of silent choice this exists to
/// avoid making for somebody. `dispatcher` names the folder explicitly
/// instead, and is required once the plain name collides.
///
/// Refuses, leaving both workspaces untouched, while
/// [`crate::lock::Lock::holder`] reports a live dispatcher over the clone's
/// current folder, or while [`any_worktree_in_use`] finds a live process
/// still working in one of its worktrees — moving the folder out from under
/// either pulls the ground out from under real, live work. An idle worktree
/// — one nothing is currently working in — moves along with the rest of the
/// folder and is repaired in place afterwards with `git worktree repair`, so
/// the main checkout's own `.git/worktrees` agrees with where it actually
/// ended up.
///
/// `to` is resolved the same way [`join_workspace`] resolves a workspace
/// name: a `project.toml` that does not parse as a [`WorkspaceToml`] — a
/// 0.6.0 repo-mode home among them, which has no `clones` key at all — is
/// refused rather than accepted as a destination.
pub(crate) fn move_clone(
    root: &Path,
    to: &str,
    dispatcher: Option<&str>,
) -> Result<WorkspaceClone> {
    require_utf8_root(root)?;
    if !crate::tracking::is_bare_filename(to) {
        bail!("`{to}` is not a workspace name — it cannot carry a path separator or a `..`");
    }
    if let Some(name) = dispatcher
        && !crate::tracking::is_bare_filename(name)
    {
        bail!("`{name}` is not a dispatcher name — it cannot carry a path separator or a `..`");
    }
    let from = workspace_clone_checked(root)?.ok_or_else(|| {
        anyhow::anyhow!(
            "{} is not listed in any workspace — there is nothing to move. Run `spoolway init \
             --setup home` first.",
            root.display()
        )
    })?;
    let from_name = from
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if from_name == to {
        bail!("{} already uses workspace {to}", root.display());
    }
    let to_workspace = crate::mux::state_root().join(to);
    let to_record = to_workspace.join(BINDING_FILE);
    if !to_record.is_file() {
        bail!(
            "no workspace named {to} exists under {}\n  check `ls {}` for the name actually \
             there, or start a fresh workspace first with `spoolway init --setup home \
             --workspace new` from a checkout",
            crate::mux::state_root().display(),
            crate::mux::state_root().display(),
        );
    }

    // Checked before either lock is taken: a dispatcher running, or a live
    // process still working in one of the clone's own worktrees, means real
    // work is live there right now, and moving the folder out from under it
    // would pull the ground out from under it. An idle
    // worktree — nothing currently working in it — is not refused here: it
    // moves along with the rest of the folder below, and is repaired in
    // place afterwards.
    let from_home = from.home_dir();
    if let Some(pid) = crate::lock::Lock::holder(&from_home.join(crate::lock::LOCK_FILE))? {
        bail!(
            "{} cannot move while a dispatcher is running over it\n  dispatcher running   pid \
             {pid}\n  the clone stays in workspace {from_name}\n  run this again once the \
             dispatch finishes",
            root.display(),
        );
    }
    if any_worktree_in_use(&from_home) {
        bail!(
            "{} cannot move while a live process is working in one of its worktrees\n  the \
             clone stays in workspace {from_name}\n  run this again once that work has \
             finished",
            root.display(),
        );
    }

    // Both workspaces' `project.toml` are read, modified and written back as
    // one operation, so a join or another move racing this one on either
    // workspace must wait rather than read a `clones` list this call is
    // about to overwrite — the same race [`join_workspace`]'s own lock
    // closes, taken here over both files at once. Locked in a fixed order —
    // by path, lowest first — regardless of which is "from" and which is
    // "to", so a move racing its own reverse can never each hold one lock
    // and wait on the other.
    let (first, second) = if from.workspace < to_workspace {
        (&from.workspace, &to_workspace)
    } else {
        (&to_workspace, &from.workspace)
    };
    let _lock_first = crate::lock::WorkspaceLock::acquire(&workspace_lock_path(first))?;
    let _lock_second = crate::lock::WorkspaceLock::acquire(&workspace_lock_path(second))?;

    let from_record = from.workspace.join(BINDING_FILE);
    let from_raw = std::fs::read_to_string(&from_record).with_context(|| {
        format!(
            "no workspace named {from_name} exists under {}",
            crate::mux::state_root().display()
        )
    })?;
    let mut from_parsed: WorkspaceToml = toml::from_str(&from_raw).with_context(|| {
        format!(
            "{} does not read as a workspace's project.toml",
            from_record.display()
        )
    })?;
    let to_raw = std::fs::read_to_string(&to_record).with_context(|| {
        format!(
            "no workspace named {to} exists under {}",
            crate::mux::state_root().display()
        )
    })?;
    let mut to_parsed: WorkspaceToml = toml::from_str(&to_raw).with_context(|| {
        format!(
            "{to} is not a workspace spoolway can move a clone into — {} does not read as a \
             workspace's project.toml (a repo-mode home's own project.toml, for instance, has \
             no `clones` list)\n  only a workspace `spoolway init --setup home` has listed can \
             be a destination — check `ls {}` for the names actually there",
            to_record.display(),
            crate::mux::state_root().display(),
        )
    })?;

    let name = dispatcher.unwrap_or(&from.dispatcher);
    let taken = to_parsed
        .clones
        .iter()
        .any(|clone| clone.dispatcher == name)
        || (WorkspaceClone {
            workspace: to_workspace.clone(),
            dispatcher: name.to_string(),
        })
        .home_dir()
        .exists();
    if taken {
        bail!(
            "workspace {to} already has a dispatcher folder named {name} — pass --dispatcher \
             <name> to move this clone's folder in under a different name",
        );
    }

    let Some(index) = from_parsed
        .clones
        .iter()
        .position(|clone| clone.root == root)
    else {
        bail!(
            "{} is not listed in workspace {from_name} any more — another command must have \
             changed it; run `spoolway init --setup home` again to see where it stands now",
            root.display(),
        );
    };
    let mut entry = from_parsed.clones.remove(index);
    entry.dispatcher = name.to_string();

    let to_home = (WorkspaceClone {
        workspace: to_workspace.clone(),
        dispatcher: name.to_string(),
    })
    .home_dir();
    std::fs::create_dir_all(
        to_home
            .parent()
            .expect("dispatchers/ is always home_dir's parent"),
    )
    .with_context(|| format!("creating {}", to_home.display()))?;

    // Every worktree cut under the clone's own dispatcher folder, by its
    // current absolute path — read before the rename below, since
    // once `from_home` is renamed away, nothing can list what
    // used to be under it any more. With `dispatch.worktree_root` retired,
    // `worktrees/` under the clone's own folder is the only place a worktree
    // is ever cut — see `crate::mux::worktree_root` — so every one of them
    // moves with it. An idle worktree is not refused above — only a live
    // process working in one is — so there can be real ones here to carry
    // across and repair.
    let moved_worktrees: Vec<PathBuf> = std::fs::read_dir(from_home.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();

    std::fs::rename(&from_home, &to_home)
        .with_context(|| format!("moving {} to {}", from_home.display(), to_home.display()))?;

    to_parsed.clones.push(entry);
    // Written destination first: once this succeeds, the clone is listed —
    // and its folder sits — at `to`, so writing the source second and
    // failing leaves it listed twice rather than not at all, a state
    // `workspace_clone_checked`'s own strict scan catches and reports rather
    // than silently resolving either way.
    if let Err(err) = write_workspace(&to_workspace, &to_parsed) {
        let _ = std::fs::rename(&to_home, &from_home);
        return Err(err);
    }
    write_workspace(&from.workspace, &from_parsed)?;

    // `repair`, unlike an ordinary `git` command, only fixes a worktree
    // whose new location it is actually told — run with no arguments from
    // `root` it repairs nothing a stale path cannot already resolve on its
    // own. Each moved worktree's own new absolute path, under `to_home`
    // rather than `from_home`, is what tells it: left unrepaired, the main
    // checkout's `.git/worktrees` still points at the path this rename just
    // carried away, and the next thing to touch that worktree — the
    // dispatcher cutting a fresh one for the same branch, most sharply —
    // finds git still convinced the branch is checked out somewhere that no
    // longer exists.
    if !moved_worktrees.is_empty() {
        let new_paths: Vec<String> = moved_worktrees
            .iter()
            .filter_map(|old| old.strip_prefix(&from_home).ok())
            .map(|rel| to_home.join(rel).display().to_string())
            .collect();
        let mut args: Vec<&str> = vec!["worktree", "repair"];
        args.extend(new_paths.iter().map(String::as_str));
        // Best-effort in outcome, not in visibility: a move that otherwise
        // succeeded must not fail over housekeeping a person can always
        // rerun by hand, but a failure here is a real thing to know about,
        // not a reason to pretend the worktrees are fine.
        if let Err(err) = run(root, "git", &args) {
            let quoted_paths: Vec<String> = new_paths
                .iter()
                .map(|path| crate::platform::quote(path))
                .collect();
            println!(
                "  worktree repair failed: {err:#}\n  run this by hand in {}:\n    git \
                 worktree repair {}",
                root.display(),
                quoted_paths.join(" "),
            );
        }
    }

    Ok(WorkspaceClone {
        workspace: to_workspace,
        dispatcher: name.to_string(),
    })
}

/// Read the value at `path` if it is there and valid — never minting,
/// unlike [`read_or_mint`]. `Ok(None)` for "nothing usable there", whether
/// that is because the file is missing or because its content fails
/// `valid`; only a real I/O error (not found is not one) is `Err`.
fn peek(path: &Path, valid: impl Fn(&str) -> bool) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            Ok(valid(trimmed).then(|| trimmed.to_string()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether `candidate` is a usable id: exactly [`ID_LEN`] lowercase
/// letters-and-digits — what every read of a checkout's own
/// `.git/spoolway-id` checks it against ([`stamped_id`]'s own mint-or-read,
/// [`project_identity`]'s lenient peek, and [`read_stamp`]'s own three-way
/// read), to tell a stamp some other id-minting run actually wrote apart
/// from one hand-edited or truncated into something
/// [`crate::mux::project_home`] could never safely build a path out of.
/// Not `pub(crate)`: every caller is inside this module.
fn is_valid_id(candidate: &str) -> bool {
    candidate.len() == ID_LEN
        && candidate
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Refuse a clone `root` that is not valid UTF-8, rather than let it reach
/// [`write_workspace`] at all. A `Path` on this platform can hold any byte
/// but `/` and NUL, but [`CloneEntry::root`] is a TOML string, so a path
/// outside that alphabet has no lossless way to be written back out —
/// writing it anyway with `from_utf8_lossy` would record a path that
/// differs from the real one at the very byte that made it invalid, so
/// [`workspace_clone`]'s exact-match lookup can never find that checkout
/// again. Called once by every writer of a `clones` entry —
/// [`create_workspace`], [`join_workspace`] and the checkout's own move —
/// rather than relying on `toml`'s own serialization failure to catch it,
/// which would name the error, not the path.
fn require_utf8_root(root: &Path) -> Result<()> {
    if root.to_str().is_none() {
        bail!(
            "{} is not valid UTF-8 — a home-mode clone's path is written into project.toml as \
             plain text, so a path with bytes that are not valid UTF-8 cannot be recorded \
             losslessly and would stop matching this checkout on every later command\n  rename \
             the checkout, or the directory it sits in, to a UTF-8-safe path first",
            root.display(),
        );
    }
    Ok(())
}

/// Refuse `root` for home mode exactly as repo mode already does: with no
/// git repository behind it, `common_git_dir` never resolves, and the
/// ancestor walk in [`Repo::root`] that `.spoolway/`'s discovery relies on to
/// stop somewhere has no git toplevel to bound it at either — a non-git
/// folder accepted into a workspace here would read "no spoolway project
/// found" from any subdirectory walked past the folder itself, exactly the
/// unbounded walk that check exists to prevent. Called by every write that
/// lists a checkout in a workspace: [`create_workspace`], [`join_workspace`]
/// and the checkout's own move — the places `spoolway init`'s own
/// home-mode menu reaches.
fn require_git_repository(root: &Path) -> Result<()> {
    if common_git_dir(root)?.is_none() {
        bail!(
            "{} is not a git repository. A home-mode workspace lists a checkout by its git \
             repository, so it needs one.\n  Run `git init` here first, then `spoolway init` \
             again.",
            root.display()
        );
    }
    Ok(())
}

/// A label safe to `join` onto `state_root()` unchanged: one normal path
/// component, never a separator or `.`/`..` — the same rule
/// `crate::tracking::is_bare_filename` enforces on a hook name for the same
/// reason. A corrupted `spoolway-label`
/// holding something like `/tmp/victim` or `../../victim` must not be able
/// to walk `project_home`'s answer outside `~/.spoolway/` at all.
fn is_valid_label(candidate: &str) -> bool {
    crate::tracking::is_bare_filename(candidate)
}

/// Turn a checkout basename into a label [`is_valid_label`] accepts,
/// changing nothing about the ones that already qualify — which is every
/// ordinary project name, so this is a no-op for almost every caller.
///
/// A Unix basename can hold any byte but `/` and NUL, which is a wider
/// alphabet than a path component this project ever joins onto
/// `state_root()` unchecked is willing to trust — see [`is_valid_label`].
/// The two characters that actually turn up in practice, `/` and `\`, are
/// folded to `-`; anything still refused after that (a bare `.`/`..`, or an
/// empty string) falls back to a fixed name rather than being minted at
/// all, so [`read_or_mint`] is never handed a value its own `valid` rule
/// would refuse the moment it was written.
fn sanitize_label(raw: &str) -> String {
    if is_valid_label(raw) {
        return raw.to_string();
    }
    let folded: String = raw
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    if is_valid_label(&folded) {
        folded
    } else {
        "project".to_string()
    }
}

/// A fresh id, drawn from [`crate::usage::fill_random`] rather than a bare
/// counter. Two processes racing to stamp one project for the first time at
/// once really does happen — see [`read_or_mint`] and
/// `concurrent_first_resolvers_agree_on_one_id` — but randomness is not
/// what settles that race; the atomic establishment in [`read_or_mint`] is.
/// What randomness buys instead: a predictable id would let one project's
/// `~/.spoolway/<label>-<id>/` be guessed from its label alone, which is one
/// presumption fewer to make about who else can read that directory.
fn generate_id() -> String {
    let mut bytes = [0u8; 4];
    crate::usage::fill_random(&mut bytes);
    let mut n = u32::from_le_bytes(bytes);
    let mut out = [0u8; ID_LEN];
    for slot in out.iter_mut().rev() {
        *slot = ID_ALPHABET[(n % 36) as usize];
        n /= 36;
    }
    // Every byte in `out` came from `ID_ALPHABET`, which is ASCII.
    String::from_utf8(out.to_vec()).expect("ID_ALPHABET is ASCII")
}

/// Read the value persisted at `path`, or atomically establish `mint()`'s
/// answer there if nothing usable is there yet.
///
/// Two processes resolving one project's identity for the first time at
/// once must agree on one answer, and a write interrupted partway (a crash,
/// a killed process) must never leave a partial file a later caller reads
/// back as though it were real. Both are the same fix: the mint is written
/// whole to a throwaway sibling file first, then [`std::fs::hard_link`]ed
/// onto `path`. A hard link either lands as the complete file it pointed at
/// or does not land at all, and fails with `AlreadyExists` rather than
/// clobbering a winner that landed first — so whichever caller's link
/// succeeds is the one true winner, and every other caller reads that
/// winner's answer back rather than minting a competing one of its own.
///
/// The `bool` says whether *this* call is the one that minted it (`true`) or
/// read back a value already there, from an earlier call in this process or
/// another one (`false`) — read straight off which branch below actually
/// ran, never inferred from a separate existence check with a race of its
/// own.
fn read_or_mint(
    path: &Path,
    valid: impl Fn(&str) -> bool,
    mint: impl FnOnce() -> String,
) -> Result<(String, bool)> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if valid(trimmed) {
            return Ok((trimmed.to_string(), false));
        }
    }
    let minted = mint();
    // `mint()` is trusted for the id (`generate_id` is valid by
    // construction) but not in general — a caller's own mint closure is
    // exactly what a label sanitizer like `sanitize_label` exists to keep
    // honest, and this is the backstop if one ever is not: writing a value
    // `valid` would refuse is worse than refusing to write at all, because
    // every later reader (`project_identity`, which only peeks) would fail
    // the same check and silently fall back as though nothing were stamped,
    // despite a file sitting right there claiming to be the stamp.
    if !valid(&minted) {
        bail!(
            "refusing to write {}: the value this would mint, {minted:?}, is not valid by its \
             own rule",
            path.display()
        );
    }
    // Unique to this call, not just to the value being minted: two threads
    // of one process racing to mint the *same* deterministic label (as two
    // callers stamping one project concurrently do) would otherwise both
    // compute the identical temp name from pid + content and stomp on each
    // other's half-written file before either reaches the hard link below.
    let mut nonce = [0u8; 8];
    crate::usage::fill_random(&mut nonce);
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        u64::from_le_bytes(nonce)
    ));
    std::fs::write(&tmp, &minted).with_context(|| format!("writing {}", tmp.display()))?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok((minted, true)),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let trimmed = existing.trim();
            if valid(trimmed) {
                Ok((trimmed.to_string(), false))
            } else {
                bail!(
                    "{} exists but does not hold a usable value: {trimmed:?}",
                    path.display()
                );
            }
        }
        Err(err) => Err(err).with_context(|| format!("linking {} into place", path.display())),
    }
}

/// The git directory a lane needs write access to for `git add`/`git commit`
/// to work in the checkout at `worktree`.
///
/// `--git-common-dir`, not the plainer `--git-dir`: in a linked worktree
/// `--git-dir` names that worktree's own `<repo>/.git/worktrees/<name>`, but
/// new blob/tree/commit objects and the branch ref itself are read and
/// written in the *common* dir, `<repo>/.git` — proven against a real
/// checkout by making each half read-only in turn: `chmod a-w
/// <repo>/.git/objects` fails `git add` there with `insufficient permission
/// for adding an object to repository database`, and `chmod a-w
/// <repo>/.git/refs` fails the following `git commit` with `cannot lock ref
/// 'HEAD'`. A grant of `--git-dir` alone would leave both writes refused.
/// `--git-common-dir` answers the directory that holds both — and, since
/// `<repo>/.git/worktrees/<name>` nests inside it, the one grant covers
/// `index.lock` too, without needing to also assemble that name from
/// `cut_worktree`'s own bookkeeping.
///
/// For a borrowed checkout — one a lane's workspace was pointed at rather
/// than one `cut_worktree` made — `worktree` is the main checkout or a
/// linked worktree cut by something else, and either way `--git-common-dir`
/// still answers that repo's one shared `.git`, never a `worktrees/<name>`
/// path assembled for a name that may not exist.
pub fn git_dir(worktree: &Path) -> Result<PathBuf> {
    let out = run(
        worktree,
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .with_context(|| format!("resolving the git directory for {}", worktree.display()))?;
    Ok(PathBuf::from(out.trim()))
}

/// The checkout `start` itself sits in — the linked worktree when `start` is
/// inside one, `root` otherwise.
///
/// `start` is inside a linked worktree exactly when it is not inside `main`
/// (`main_checkout`'s answer for it): a linked worktree is a sibling of the
/// main checkout on disk, never a descendant of it, so `start` failing to
/// sit under `main` is what tells the two apart, no second `git rev-parse`
/// needed. In that case the worktree's own top is the nearest ancestor of
/// `start` carrying a `.git` — a file there, pointing back at the main
/// checkout's git dir, where the main checkout's own `.git` is a directory.
///
/// Every other case is the main checkout itself, so `root` already names it
/// — including the case a naive `.git`-ancestor walk from `start` gets
/// wrong: a project whose `.spoolway/` sits below the checkout's own top
/// (`root` found via the ancestor search in [`Repo::root`], not through git
/// at all) still has its single `.git` higher up, at the checkout's real
/// top, which is not `root` and would be the wrong answer to hand back as
/// `checkout`.
fn checkout_of(start: &Path, root: &Path, main: Option<&Path>) -> PathBuf {
    match main {
        Some(main) if !start.starts_with(main) => start
            .ancestors()
            .find(|dir| dir.join(".git").exists())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf()),
        _ => root.to_path_buf(),
    }
}

/// [`checkout_of`], aliased onto `root` in home mode.
///
/// A branch-local `.spoolway/` is the whole reason [`checkout_of`] answers a
/// linked worktree's own path rather than `root`'s — a lane reads the
/// tracked setup its own branch actually carries. A workspace's `config/`
/// has no such thing: it is never tracked, never on any branch, and shared
/// identically by every clone and every worktree of this one, so there is
/// nothing branch-local left to read `checkout` for. Aliasing it here,
/// rather than teaching every `repo.checkout`-based accessor to make this
/// same check on every call, is what lets a lane's own worktree resolve the
/// workspace's prompts and pipelines exactly as `root` does — see
/// [`workspace_clone`]'s own doc for why `root`, not `start` or the
/// worktree path `checkout_of` might otherwise answer, is what a clone
/// entry's own `root` is matched against.
fn checkout_for(start: &Path, root: &Path, main: Option<&Path>) -> PathBuf {
    if workspace_clone(root).is_some() {
        return root.to_path_buf();
    }
    checkout_of(start, root, main)
}

/// The branch checked out in `dir`.
///
/// This is asked of a worktree, not only of the main checkout: a task is based
/// on the branch of the checkout it was queued in, which is how several plans
/// stay independent while sharing one queue and one dispatcher.
///
/// `branch --show-current` rather than `rev-parse --abbrev-ref HEAD`: it
/// answers correctly on a repo with no commits yet, and returns empty on a
/// detached HEAD instead of the literal string "HEAD".
pub fn branch_at(dir: &Path) -> Result<String> {
    let branch = run(dir, "git", &["branch", "--show-current"])?
        .trim()
        .to_string();
    if branch.is_empty() {
        bail!(
            "{} is not on a branch (detached HEAD). Work is based on the branch of the \
             checkout it is queued in, so check one out first.",
            dir.display()
        );
    }
    Ok(branch)
}

/// `~/.spoolway/` in the spelling `Repo::root`'s canonicalized `start`
/// compares against — resolved when it exists, as given when it does not,
/// since then no ancestor can equal it either way.
fn global_state_root() -> PathBuf {
    let dir = crate::mux::state_root();
    dir.canonical().unwrap_or(dir)
}

/// The branch `dir` has out, for an error message only: `branch_at` refuses
/// a detached HEAD with advice of its own, and the message this feeds wants
/// to name what is checked out rather than stop on it.
fn branch_or_detached(dir: &Path) -> String {
    match run(dir, "git", &["branch", "--show-current"]) {
        Ok(branch) if !branch.trim().is_empty() => branch.trim().to_string(),
        _ => "(detached HEAD)".to_string(),
    }
}

/// `dir`'s best-guess default branch, without switching it off whatever is
/// actually checked out: `origin`'s own recorded `HEAD` when there is one —
/// set by an ordinary `git clone`, and by nothing this binary writes — or
/// else whichever of `main`/`master` exists as a local branch. A guess:
/// a repo whose default branch is named otherwise, with no `origin/HEAD`,
/// is not caught.
/// `None` when neither answers, which callers read as "nothing to check".
fn default_branch(dir: &Path) -> Option<String> {
    if let Ok(out) = run(
        dir,
        "git",
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let name = out.trim();
        if let Some(branch) = name.strip_prefix("origin/") {
            return Some(branch.to_string());
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|candidate| {
            run(
                dir,
                "git",
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{candidate}"),
                ],
            )
            .is_ok()
        })
        .map(str::to_string)
}

/// `dir`'s default branch, if it tracks `.spoolway/` there — asked with
/// `git cat-file`, against the branch's own tree, so a checkout sitting on
/// some other branch right now never has to switch onto it to find out.
/// `require_git_repository` has already refused anything with no git
/// repository behind it by the time [`Placement::choose_any`] calls this, so
/// `dir` always resolves a toplevel here.
///
/// Acceptance criterion: a repo-mode project's `.spoolway/` only has to be
/// *tracked* on the default branch to make a home-mode setup here wrong,
/// not checked out on it right now — a checkout on an orphan or feature
/// branch, with no `.spoolway/` of its own, used to pass the ordinary
/// [`crate::config::tracked_setup_dir_in`] check and accept `--setup home`,
/// breaking every command the moment the default branch came back.
pub(crate) fn default_branch_tracking_spoolway(dir: &Path) -> Option<String> {
    let branch = default_branch(dir)?;
    run(
        dir,
        "git",
        &[
            "cat-file",
            "-e",
            &format!("{branch}:{}", crate::config::STATE_DIR),
        ],
    )
    .ok()
    .map(|_| branch)
}

fn git_toplevel(dir: &Path) -> Result<PathBuf> {
    let out = run(dir, "git", &["rev-parse", "--show-toplevel"])?;
    // The same spelling rule as `main_checkout`: git's answer, in the form
    // `canonicalize` would give, so paths derived from either compare equal.
    let top = PathBuf::from(out.trim());
    top.canonical()
        .with_context(|| format!("resolving {}", top.display()))
}

/// The git toplevel of `cwd`, exactly as git spells it, byte for byte.
/// [`run`] reads stdout with `from_utf8_lossy`, which would turn a checkout
/// path holding invalid UTF-8 into a different, valid one before
/// [`require_utf8_root`] could see it — so `init` stored the lossy spelling
/// instead of refusing. Uncanonicalized, as `init` has always taken it.
pub fn toplevel_raw(cwd: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .context("running `git rev-parse --show-toplevel`")?;
    if !output.status.success() {
        bail!(
            "`git rev-parse --show-toplevel` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut bytes = output.stdout;
    while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
        bytes.pop();
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// Run a command in `cwd` and return its stdout, or an error carrying stderr.
///
/// Every `git` call and every `herdr` call the multiplexer backend makes runs
/// through here, so a test can count them — see [`PROCESS_RUNS`]. Other
/// processes, such as a lane's agent started by `herdr agent start` or a
/// `kill`, are started elsewhere and are not counted.
pub fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    #[cfg(test)]
    if let Ok(mut runs) = PROCESS_RUNS.lock() {
        runs.push((cwd.to_path_buf(), std::thread::current().id()));
    }
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running `{program} {}`", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every process [`run`] has started so far in this process, as the `cwd`
/// it ran in and the thread that started it — test-only. Scoped by `cwd` so
/// a test under its own scratch directory counts only its own processes,
/// where a stand-in on the process-global `PATH` raced every other test
/// running beside it. Scoped by thread so a test can tell the processes its
/// own thread paid for from the ones the board's reader thread started.
#[cfg(test)]
static PROCESS_RUNS: std::sync::Mutex<Vec<(PathBuf, std::thread::ThreadId)>> =
    std::sync::Mutex::new(Vec::new());

/// How many processes [`run`] has started with `cwd` under `dir` so far, on
/// any thread — see [`PROCESS_RUNS`].
#[cfg(test)]
pub(crate) fn runs_under(dir: &Path) -> usize {
    PROCESS_RUNS
        .lock()
        .map(|runs| runs.iter().filter(|(cwd, _)| cwd.starts_with(dir)).count())
        .unwrap_or(0)
}

/// [`runs_under`], counting only the processes the calling thread started.
#[cfg(test)]
pub(crate) fn runs_here_under(dir: &Path) -> usize {
    let here = std::thread::current().id();
    PROCESS_RUNS
        .lock()
        .map(|runs| {
            runs.iter()
                .filter(|(cwd, thread)| *thread == here && cwd.starts_with(dir))
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte range of `fn <marker>(`'s own body in `text` — from its
    /// opening `{` to the matching closing `}`, found by counting braces
    /// rather than indentation, so it survives a reformat. `None` when no
    /// function by that name is found at all, which the caller treats as
    /// "nothing to exempt" rather than a silent no-op.
    fn fn_body_range(text: &str, marker: &str) -> Option<std::ops::Range<usize>> {
        let start = text.find(marker)?;
        let open = text[start..].find('{')? + start;
        let mut depth = 0usize;
        for (offset, ch) in text[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(open..open + offset + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The rule the `setup-dir-accessor` task exists to enforce: nothing
    /// outside [`crate::config::setup_dir_in`], [`crate::config::under_setup`],
    /// [`Repo::setup_dir`], [`Repo::under_setup`] and
    /// [`crate::config::tracked_setup_dir_in`] — the five functions this
    /// folder is ever reached through — joins [`crate::config::STATE_DIR`],
    /// one of `crate::config`'s other setup-path constants, or a literal
    /// `.spoolway/…` path straight onto a checkout or root. The fifth is
    /// `home-mode-discovery`'s own addition: the one place `bind` asks
    /// whether a checkout's tracked `.spoolway/` is real, rather than where
    /// its setup should be read from, which is the question every other
    /// caller — including `setup_dir_in` itself — asks instead. A
    /// behavioural test only covers the callers it happens to drive; this
    /// reads the source instead, the same way
    /// `commands::tests::nothing_builds_a_prompt_path_except_the_one_function_that_should`
    /// already does for prompt paths, so a new offender fails here on the
    /// next `cargo test` rather than being noticed only once the setup
    /// folder actually needs to move.
    ///
    /// Tests are exempt — a fixture planting a `.spoolway/` directory to
    /// simulate a project is not a reader reaching for the real one — and so
    /// is prose: a comment or a message's own text saying `.spoolway/hooks`
    /// is not a join. Both are approximated the same blunt way: a comment
    /// line is dropped before the search, and everything from a file's own
    /// top-level `mod tests {` onward is dropped with it, since that is
    /// where every fixture in this codebase lives. The five functions
    /// themselves are excised by [`fn_body_range`] rather than by skipping
    /// their whole file, so a sixth join written anywhere else in
    /// `config.rs` or `repo.rs` still fails this.
    #[test]
    fn nothing_joins_state_dir_except_the_one_accessor() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        let mut pending = vec![src];
        let mut sources = Vec::new();
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("reading src/") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    sources.push(path);
                }
            }
        }

        // The setup-path constants `crate::config` exports, joined onto a
        // checkout only by [`crate::config::under_setup`]/
        // [`Repo::under_setup`] — see their own docs.
        const CONSTANTS: &[&str] = &[
            "STATE_DIR",
            "PROMPTS_DIR",
            "TASK_TEMPLATES_DIR",
            "ROUTINES_DIR",
            "JOBS_FILE",
        ];

        for path in sources {
            let name = path.file_name().unwrap().to_string_lossy().to_string();

            // `mux.rs` and `models.rs` join a `.spoolway/…` literal onto a
            // different directory entirely: `crate::mux::home()`, the
            // machine's own `~/.spoolway/` (project homes, the shared
            // dispatch workspace, the vendored model-price cache) rather
            // than a checkout's tracked control plane — see
            // [`crate::mux::state_root`]. Out of scope for this rule, which
            // is only about the checkout-relative folder `Repo::setup_dir`
            // names; changing where runtime state lives is this task's own
            // non-goal. Nothing else is exempt by filename: `config.rs` and
            // `repo.rs` are scanned like any other file, with only the five
            // accessor bodies themselves cut out below.
            if matches!(name.as_str(), "mux.rs" | "models.rs") {
                continue;
            }

            let mut text = std::fs::read_to_string(&path).expect("reading a source file");

            // Everything from a top-level `mod tests {` on is a fixture,
            // not a reader — see the doc above.
            if let Some(at) = text.find("\nmod tests {") {
                text.truncate(at);
            }

            // Cut the accessor's own body out before scanning, rather than
            // skipping the whole file that defines it — see the doc above.
            // `config.rs`'s free `under_setup`/`setup_dir_in`/
            // `tracked_setup_dir_in` and `repo.rs`'s methods of the first
            // two names are unambiguous within a single file's text, so the
            // same five markers find the right one in whichever file
            // actually defines it.
            for marker in [
                "fn setup_dir_in(",
                "fn under_setup(",
                "fn setup_dir(",
                "fn tracked_setup_dir_in(",
            ] {
                if let Some(range) = fn_body_range(&text, marker) {
                    text.replace_range(range, "");
                }
            }
            let body = text;

            // Line by line would miss the shape rustfmt actually produces,
            // where `.join(` lands on its own line — so this searches the
            // text with whitespace removed, and recovers the line number
            // from the offset afterwards, exactly as the prompt-path test
            // does.
            let mut flat = String::with_capacity(body.len());
            let mut lines = Vec::with_capacity(body.len());
            for (number, line) in body.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for character in line.chars().filter(|c| !c.is_whitespace()) {
                    flat.push(character);
                    lines.push(number + 1);
                }
            }

            for constant in CONSTANTS {
                for needle in [
                    format!(".join({constant})"),
                    format!(".join(crate::config::{constant})"),
                ] {
                    for (at, _) in flat.match_indices(&needle) {
                        offenders.push(format!("{name}:{}", lines[at]));
                    }
                }
            }
            // A literal `.spoolway/…` handed straight to `.join(`, rather
            // than through the constants above — the same offence spelled
            // without the constant's name. The trailing slash matters: a
            // name that merely starts with `.spoolway` (`update.rs`'s own
            // `.spoolway-update-no-project.lock`, named beside a checkout,
            // never inside one) is a different file, not this directory.
            for (at, _) in flat.match_indices(".join(\".spoolway/") {
                offenders.push(format!("{name}:{}", lines[at]));
            }
        }

        assert!(
            offenders.is_empty(),
            "join a setup-path constant (or a `.spoolway/` literal) onto a checkout or root \
             only through `crate::config::setup_dir_in`/`under_setup` (or \
             `Repo::setup_dir`/`under_setup`):\n  {}",
            offenders.join("\n  ")
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        run(dir, "git", args).unwrap_or_else(|e| panic!("git {args:?} in {dir:?}: {e:#}"))
    }

    /// One separator, for the assertions that read a path back out of git's
    /// own output.
    ///
    /// git prints a Windows path its own way — `C:/Users/runneradmin/…` —
    /// while `Path::display` spells the same directory with backslashes, so
    /// a substring test between the two compares the separator rather than
    /// the directory and misses every time. Only the `git worktree list`
    /// assertions need this: everywhere else the text under test is a
    /// spoolway message built with `display()` on both sides.
    fn slashed(text: impl AsRef<str>) -> String {
        text.as_ref().replace('\\', "/")
    }

    /// A scratch `$HOME`, canonicalized the way `Repo::root` canonicalizes
    /// the path it walks up from, so a `.spoolway` planted under it compares
    /// equal to what discovery sees.
    fn scratch_home(name: &str) -> (PathBuf, crate::scratch::ScratchRoot) {
        let home = crate::scratch::root(&format!("repo-test-home-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let canonical = home.canonical().unwrap();
        (canonical, home)
    }

    /// `Repo::discover`, under a scratch home of its own — never the real
    /// `~/.spoolway/`. Binding is automatic now (`Repo::discover` calls
    /// `bind` itself, criterion 7: nothing recorded, no stamp, binds on its
    /// own), so there is nothing left to claim first. The home's guard comes
    /// back with the `Repo`: its home lives inside that directory, and a test
    /// writing under `repo.home` after the guard dropped here would recreate
    /// it with nothing left to remove it.
    fn discover_registered(work: &Path) -> Result<(Repo, crate::scratch::ScratchRoot)> {
        discover_registered_as(work, work)
    }

    /// The same, started from `start` — a linked worktree of `_project`, in
    /// the tests that need one. `_project` is unused now that binding is
    /// automatic; kept as a parameter so every call site naming the project
    /// a worktree belongs to still reads that way.
    fn discover_registered_as(
        _project: &Path,
        start: &Path,
    ) -> Result<(Repo, crate::scratch::ScratchRoot)> {
        let (home, home_guard) = scratch_home("registered");
        let repo = crate::platform::test_home::with_home(&home, || Repo::discover(start))?;
        Ok((repo, home_guard))
    }

    /// A bare "origin", a checkout wired to it, and spoolway state in the
    /// checkout. Real git throughout: these are the behaviours that only break
    /// against the real thing.
    fn fixture(name: &str) -> (PathBuf, PathBuf, crate::scratch::ScratchRoot) {
        let base = crate::scratch::root(&format!("repo-test-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let origin = base.join("origin.git");
        let work = base.join("work");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();

        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(&work, &["init", "-q", "-b", "plan/x"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );

        // The tracked control plane, as a real project has: this is precisely
        // what makes a task worktree carry a `.spoolway` directory of its
        // own. The queue and archive are not part of it any more — see
        // `Repo::home` — so there is nothing runtime to seed here.
        let state = work.join(crate::config::STATE_DIR);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("config.toml"), "").unwrap();
        std::fs::write(work.join("README"), "hello\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "root"]);

        (origin, work, base)
    }

    /// The retention sweep is allowed to delete anything on this list, so
    /// the checkout's tracked prompt templates must never appear on it. They
    /// did once, when `prompts/` still named the composed-prompt directory,
    /// and the sweep deleted checked-in files.
    #[test]
    fn the_tracked_prompt_templates_are_not_a_byproduct_directory() {
        let (_origin, work, _base_guard) = fixture("byproducts");
        let (repo, _home) = discover_registered(&work).unwrap();
        let dirs = repo.byproduct_dirs();

        assert!(
            !dirs.contains(&repo.prompts_dir()),
            "the checkout's tracked .spoolway/prompts/ is swept: {dirs:?}"
        );
        assert!(
            !dirs.contains(&repo.overrides_dir()),
            "the patch layer is swept, and a patch older than retention.days \
             would silently change how work runs: {dirs:?}"
        );
        assert!(
            dirs.contains(&repo.system_prompts_dir()),
            "the composed system prompts are never swept: {dirs:?}"
        );
        for dir in &dirs {
            assert!(
                dir.starts_with(repo.home()),
                "a swept directory outside the state home: {dir:?}"
            );
        }
    }

    #[test]
    fn discover_from_a_linked_worktree_finds_the_project_not_the_worktree() {
        let (_origin, work, _base_guard) = fixture("worktree");
        let wt = work.parent().unwrap().join("task-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        // The worktree carries a tracked .spoolway/, which is exactly the trap:
        // walking up for it would stop here and find an empty queue.
        assert!(wt.join(crate::config::STATE_DIR).is_dir());

        let (repo, _home) = discover_registered_as(&work, &wt).unwrap();
        assert_eq!(
            repo.root.canonical().unwrap(),
            work.canonical().unwrap(),
            "a lane running in its worktree must resolve to the project"
        );
        assert_eq!(
            repo.checkout.canonical().unwrap(),
            wt.canonical().unwrap(),
            "but the tracked control plane is read from the worktree's own \
             checkout, not the main one's"
        );

        git(
            &work,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
    }

    /// A project stamped by a spoolway built before `spoolway-root` existed
    /// carries only the id and label files in its common git directory —
    /// `main_checkout`'s verified parent trick has to be what resolves it,
    /// never a recorded path that was never written. Both the main checkout
    /// and a linked worktree of it must still bind to the one home already
    /// on record, and a repeat `init` must never mint a second one.
    #[test]
    fn a_project_stamped_before_spoolway_root_existed_still_binds_from_every_checkout() {
        let (_origin, work, _base_guard) = fixture("pre-root-file-stamp");
        let common = work.join(".git");
        let (home, _home_guard) = scratch_home("pre-root-file-stamp");

        let home_dir = crate::platform::test_home::with_home(&home, || {
            // Set up for real first, exactly as a 0.6.0 binary would, then
            // take away only what 0.6.0 never wrote — the recorded path a
            // newer binary's `stamped_id` adds alongside the id and label.
            let home_dir = Repo::discover(&work).unwrap().home;
            std::fs::remove_file(common.join(ROOT_FILE)).unwrap();
            home_dir
        });

        let wt = work.parent().unwrap().join("pre-root-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        crate::platform::test_home::with_home(&home, || {
            let home_from_main = Repo::discover(&work).unwrap().home;
            let home_from_worktree = Repo::discover(&wt).unwrap().home;
            assert_eq!(
                home_from_main, home_dir,
                "the main checkout must still bind to the home already on record"
            );
            assert_eq!(
                home_from_worktree, home_dir,
                "a linked worktree of a pre-upgrade stamp must bind to the same home, \
                 not mint a second one"
            );

            // A repeat `init` must read the same stamp back, not mint a
            // second id alongside it.
            let (id_again, minted_again) = stamped_id(&work).unwrap().unwrap();
            assert!(
                !minted_again,
                "a pre-upgrade stamp must be read, not re-minted"
            );
            assert_eq!(
                std::fs::read_to_string(common.join(ID_FILE))
                    .unwrap()
                    .trim(),
                id_again
            );
        });

        git(
            &work,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
    }

    /// A linked worktree's own git dir, `.git/worktrees/<name>`, holds
    /// `index.lock` — but not the objects a `git add` writes or the branch
    /// ref a `git commit` moves, which live in the main checkout's shared
    /// `.git` instead. So the grant a lane needs is the *main* checkout's
    /// `.git`, whether it is asked of the linked worktree or of the main
    /// checkout itself — the two resolve to the one path a real commit made
    /// from inside the linked worktree actually writes into, existing and
    /// identical either way, never a `worktrees/<name>` path that holds only
    /// half of what a commit needs.
    #[test]
    fn git_dir_resolves_a_linked_worktree_to_the_shared_main_git_dir() {
        let (_origin, work, _base_guard) = fixture("git-dir");
        let wt = work.parent().unwrap().join("task-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/git-dir",
                wt.to_str().unwrap(),
            ],
        );

        let main_dir = git_dir(&work).unwrap();
        let linked_dir = git_dir(&wt).unwrap();

        assert!(main_dir.is_dir(), "{main_dir:?} must exist");
        // Both sides canonicalized: git answers with the fully resolved path,
        // where `work` came from the scratch root as the platform hands it
        // over. On Windows that is the 8.3 short form — `RUNNER~1` against
        // git's `runneradmin` — and the two name one directory but compare
        // unequal.
        assert_eq!(
            main_dir.canonical().unwrap(),
            work.join(".git").canonical().unwrap()
        );
        assert_eq!(
            linked_dir, main_dir,
            "a linked worktree's git dir is the *common* dir it shares with \
             the main checkout, not its own worktrees/<name> subdirectory — \
             that is where a commit made from inside it actually writes"
        );

        // Proof, not assertion by construction: a real commit from inside the
        // linked worktree must land its object and its ref move under the
        // resolved `main_dir` — the exact grant a lane is given.
        std::fs::write(wt.join("f.txt"), "content").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "from the linked worktree"]);
        let head_after = git(&wt, &["rev-parse", "HEAD"]);
        assert!(
            main_dir
                .join("objects")
                .join(&head_after.trim()[..2])
                .join(&head_after.trim()[2..])
                .is_file(),
            "the commit's object must land under the resolved git dir, not \
             the linked worktree's own subdirectory"
        );

        git(
            &work,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
    }

    /// A `--separate-git-dir` clone's common git directory can sit anywhere
    /// at all, so `main_checkout`'s old parent-of-common-dir trick named the
    /// wrong directory for every linked worktree of one. `spoolway init`
    /// records the real checkout once, via `stamped_id`, and this is that
    /// record surviving a lookup made from a linked worktree whose own
    /// `--git-common-dir` points at the very same file.
    #[test]
    fn main_checkout_resolves_a_separate_git_dirs_linked_worktree() {
        let base = crate::scratch::root("repo-test-separate-git-dir");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Named `git` on purpose, not `foo.git`: the parent `elsewhere` is
        // then plainly no checkout. A conventionally named one is covered by
        // `common_git_dir_tells_bare_from_a_separate_git_dir_by_asking_git_not_the_name`.
        let git_dir = base.join("elsewhere").join("git");
        std::fs::create_dir_all(git_dir.parent().unwrap()).unwrap();
        let work = base.join("work");
        run(
            &base,
            "git",
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", git_dir.display()),
                work.to_str().unwrap(),
            ],
        )
        .unwrap();
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("f.txt"), "x").unwrap();
        git(&work, &["add", "f.txt"]);
        git(&work, &["commit", "-q", "-m", "x"]);

        // Before `spoolway init` has ever run here, there is nothing
        // recorded yet, and the parent trick does not verify (the common
        // directory's parent is not itself a git repository) — so this must
        // answer `None`, not a wrong guess. Guessing the unverified parent
        // here used to be `main_checkout`'s own fallback, and it broke the
        // very first `spoolway init` in a fresh clone like this one: `None`
        // is what lets `init_root` fall through to `toplevel_raw` instead,
        // which answers `work` correctly — see
        // `init_root_falls_through_to_toplevel_for_an_unstamped_separate_git_dir_clone`
        // below for that end-to-end path.
        assert!(
            main_checkout(&work).is_none(),
            "sanity check: an unstamped --separate-git-dir checkout must \
             resolve to nothing yet, not a wrong guess"
        );

        stamped_id(&work).unwrap(); // stands in for `spoolway init`

        assert_eq!(
            main_checkout(&work).unwrap().canonical().unwrap(),
            work.canonical().unwrap(),
            "the main checkout must resolve to itself once stamped"
        );

        let wt = base.join("task-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        assert_eq!(
            main_checkout(&wt).unwrap().canonical().unwrap(),
            work.canonical().unwrap(),
            "a linked worktree of a --separate-git-dir clone must resolve \
             to the main checkout, not wherever its common git directory \
             happens to live"
        );
    }

    /// `common_git_dir` used to guess "bare repository" from the common
    /// directory's own name — anything not literally `.git` or `git` — which
    /// read a real, non-bare `--separate-git-dir` clone named the ordinary
    /// way (`repos/foo.git`, a bare `elsewhere`) as having no git repository
    /// at all. Asked of git directly instead, `--is-bare-repository`, so
    /// both a conventionally-named separate git directory and a real bare
    /// repository are told apart correctly regardless of what either is
    /// called.
    #[test]
    fn common_git_dir_tells_bare_from_a_separate_git_dir_by_asking_git_not_the_name() {
        let base = crate::scratch::root("repo-test-common-git-dir-naming");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repos")).unwrap();

        // Named the way a person actually would: `foo.git`, not `git`.
        let git_dir = base.join("repos").join("foo.git");
        let work = base.join("work");
        run(
            &base,
            "git",
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", git_dir.display()),
                work.to_str().unwrap(),
            ],
        )
        .unwrap();
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        assert!(
            common_git_dir(&work).unwrap().is_some(),
            "a real, non-bare --separate-git-dir clone must resolve even \
             when its git directory is not named `.git` or `git`"
        );

        // A real bare repository, also named `foo.git` — the exact name
        // that used to be mistaken for a real, resolvable git directory.
        let bare = base.join("repos").join("bare-example.git");
        run(
            &base,
            "git",
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        )
        .unwrap();
        assert!(
            common_git_dir(&bare).unwrap().is_none(),
            "a real bare repository must still answer `None`, whatever it is named"
        );
    }

    /// The one fact every `checkout:` line and its `--json` twin read off of:
    /// a worktree's own absolute path and the branch it has out — not the
    /// project's.
    #[test]
    fn checkout_note_names_the_worktree_and_its_branch() {
        let (_origin, work, _base_guard) = fixture("checkout-note");
        let wt = work.parent().unwrap().join("note-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/note",
                wt.to_str().unwrap(),
            ],
        );

        let (repo, _home) = discover_registered_as(&work, &wt).unwrap();
        let note = repo
            .checkout_note()
            .unwrap()
            .expect("checkout and root differ, so there is a note");
        assert_eq!(note.path.canonical().unwrap(), wt.canonical().unwrap());
        assert_eq!(note.branch, "task/note");

        git(
            &work,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
    }

    /// The main checkout is the common case, and printing anything there
    /// would be noise — so there is no note to print at all.
    #[test]
    fn checkout_note_is_none_in_the_main_checkout() {
        let (_origin, work, _base_guard) = fixture("checkout-note-main");
        let (repo, _home) = discover_registered(&work).unwrap();
        assert!(repo.checkout_note().unwrap().is_none());
    }

    #[test]
    fn discover_from_the_project_itself_is_unchanged() {
        let (_origin, work, _base_guard) = fixture("plain");
        let (repo, _home) = discover_registered(&work).unwrap();
        assert_eq!(
            repo.checkout.canonical().unwrap(),
            repo.root.canonical().unwrap(),
            "in the main checkout, checkout and root are the same directory"
        );
        assert_eq!(repo.root.canonical().unwrap(), work.canonical().unwrap());
    }

    /// What lets `doctor` run on the project someone is trying to fix.
    #[test]
    fn a_config_that_does_not_parse_stops_discovery_only_for_the_strict_path() {
        let (_origin, work, _base_guard) = fixture("unparsable");
        std::fs::write(
            work.join(crate::config::STATE_DIR).join("config.toml"),
            "this is not = = toml\n",
        )
        .unwrap();

        let (home, _home_guard) = scratch_home("unparsable");
        let (strict, lenient) = crate::platform::test_home::with_home(&home, || {
            (Repo::discover(&work), Repo::discover_lenient(&work))
        });
        assert!(strict.is_err(), "every other command dies");

        let (repo, err, home_error) = lenient.unwrap();
        assert!(home_error.is_none(), "the home itself resolved fine here");
        assert_eq!(repo.root.canonical().unwrap(), work.canonical().unwrap());
        let err = err.expect("the parse error is handed back, not swallowed");
        assert!(
            format!("{err:#}").contains("config.toml"),
            "the error names the file to fix: {err:#}"
        );
    }

    /// A real failure resolving a project's stamped home — a permissions
    /// problem reading `spoolway-id`, standing in for one — must not take
    /// `discover_lenient` down with it: `doctor` is the one command whose
    /// entire point is to run when something about the project is broken,
    /// and it cannot report a finding about a `Repo` it was never handed.
    #[test]
    fn a_home_resolution_failure_does_not_stop_lenient_discovery() {
        use std::os::unix::fs::PermissionsExt;
        let (_origin, work, _base_guard) = fixture("home-resolution-failure");
        stamped_id(&work).unwrap();
        let id_file = work.join(".git").join("spoolway-id");
        let mut perms = std::fs::metadata(&id_file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&id_file, perms.clone()).unwrap();

        let lenient = Repo::discover_lenient(&work);

        // Restore before asserting, so a failed assertion does not leave a
        // file this test's own cleanup cannot remove.
        perms.set_mode(0o644);
        std::fs::set_permissions(&id_file, perms).unwrap();

        let (_repo, config_error, home_error) =
            lenient.expect("a home resolution failure is a finding, not a fatal error");
        let home_error = home_error.expect("the real read failure is handed back, not swallowed");
        assert!(
            format!("{home_error:#}").contains("spoolway-id"),
            "the error names the file that could not be read: {home_error:#}"
        );
        // `Config::load` would fail for the exact same reason — it resolves
        // the overrides layer through the same call — so reading it the
        // ordinary way here would report one real problem as two. The
        // tracked file itself is fine, so `config_error` must say so.
        assert!(
            config_error.is_none(),
            "a broken home must not cascade into a false config failure: {config_error:?}"
        );
    }

    /// Two plans in flight at once: each has a branch and a worktree of its
    /// own, and one dispatcher serves both. So a branch being looked up is
    /// usually not the one the dispatcher's own checkout is on.
    #[test]
    fn a_branch_checked_out_in_another_worktree_is_found_there() {
        let (_origin, work, _base_guard) = fixture("worktree-lookup");
        let (repo, _home) = discover_registered(&work).unwrap();

        let plan_y = work.parent().unwrap().join("plan-y");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "plan/y",
                plan_y.to_str().unwrap(),
            ],
        );
        assert_eq!(
            repo.worktree_for("plan/y")
                .unwrap()
                .unwrap()
                .canonical()
                .unwrap(),
            plan_y.canonical().unwrap(),
        );
        assert!(
            repo.worktree_for("plan/nobody-has-this").unwrap().is_none(),
            "a branch nobody has out has no worktree"
        );

        git(
            &work,
            &["worktree", "remove", "--force", plan_y.to_str().unwrap()],
        );
    }

    /// `retain` sweeps the archive by age with no regard for a queued
    /// dependent still naming the swept task, so a dependent parked for
    /// longer than `retention.days` comes back to find its dependency's file
    /// gone while the branch it has to be cut from is still there. The
    /// branch is what the cut needs; the file only said which one — and with
    /// neither there, the error names both (lifecycle review finding 5).
    #[test]
    fn a_dependency_whose_file_aged_out_of_the_archive_still_resolves_to_its_branch() {
        let (_origin, work, _base_guard) = fixture("dependency-branch-fallback");
        git(&work, &["branch", "task/aged"]);
        let (repo, _home) = discover_registered(&work).unwrap();
        assert!(
            !repo.queue_dir().join("aged.md").exists()
                && !repo.archive_dir().join("aged.md").exists(),
            "the task file is what this test does without"
        );

        assert_eq!(
            repo.dependency_branch("aged").unwrap(),
            "task/aged",
            "the branch is there, so it is what the cut gets"
        );

        let err = repo.dependency_branch("gone").unwrap_err().to_string();
        assert!(
            err.contains("`gone`") && err.contains("`task/gone`"),
            "neither the file nor the branch exists, and the error says which \
             of each it looked for: {err}"
        );
    }

    #[test]
    fn a_repo_with_no_remote_reports_so() {
        let base = crate::scratch::root("repo-test-noremote");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(crate::config::STATE_DIR)).unwrap();
        git(&base, &["init", "-q", "-b", "work"]);

        let (repo, _home) = discover_registered(&base).unwrap();
        assert!(
            !repo.has_remote(),
            "local-only repos must not attempt a push"
        );
    }

    /// A branch nobody here has ever fetched still answers `git ls-remote`
    /// honestly — the check a base only `origin` has depends on to be
    /// accepted at all, and a name neither place has answers `false`
    /// exactly the same way.
    #[test]
    fn remote_branch_exists_asks_origin_directly_rather_than_a_local_fetch() {
        let (origin, work, _base_guard) = fixture("remote-branch-exists");
        git(&work, &["push", "-q", "origin", "plan/x:main"]);
        git(&origin, &["branch", "task/remote-only", "main"]);
        let (repo, _home) = discover_registered(&work).unwrap();

        assert!(
            repo.remote_branch_exists("task/remote-only"),
            "origin has the branch, even though this checkout never fetched it"
        );
        assert!(
            !repo.remote_branch_exists("task/nowhere"),
            "neither place has this one"
        );
    }

    /// `git ls-remote` reads a bare pattern as a glob matched against the
    /// *tail* of a ref, starting at a `/` boundary — so a name that is a
    /// real branch's own tail, `gh-412-checkout` against a real
    /// `task/gh-412-checkout`, must not read as a match. Passed a full
    /// `refs/heads/<branch>` pattern instead, which only ever matches the
    /// exact ref (review finding 1).
    #[test]
    fn remote_branch_exists_does_not_match_a_real_branchs_own_suffix() {
        let (origin, work, _base_guard) = fixture("remote-branch-suffix");
        git(&work, &["push", "-q", "origin", "plan/x:main"]);
        git(&origin, &["branch", "task/gh-412-checkout", "main"]);
        let (repo, _home) = discover_registered(&work).unwrap();

        assert!(
            !repo.remote_branch_exists("gh-412-checkout"),
            "`gh-412-checkout` is a suffix of the real branch, not the branch itself"
        );
        assert!(repo.remote_branch_exists("task/gh-412-checkout"));
    }

    /// The trap `checkout_of` used to fall into: a git repo whose single
    /// `.git` sits above a `.spoolway/` planted in a subdirectory of it.
    /// `root` is found by `Repo::root`'s ancestor search, not through git —
    /// and a `checkout` computed by naively walking up `start` for the
    /// nearest `.git` would land one level higher, outside the very project
    /// `root` names.
    #[test]
    fn a_spoolway_project_nested_below_its_gits_own_top_reads_checkout_as_root() {
        let base = crate::scratch::root("repo-test-nested-state");
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("sub");
        std::fs::create_dir_all(sub.join(crate::config::STATE_DIR)).unwrap();
        git(&base, &["init", "-q", "-b", "work"]);

        let (repo, _home) = discover_registered(&sub).unwrap();
        assert_eq!(
            repo.root.canonical().unwrap(),
            sub.canonical().unwrap(),
            "root is found via the .spoolway ancestor search here, not through git"
        );
        assert_eq!(
            repo.checkout, repo.root,
            "checkout must not land at the git top above root — there is no \
             linked worktree here, only one checkout, and root already names it"
        );
    }

    /// A workspace holds this checkout's repository when any of its clone
    /// entries shares this checkout's root commit — not only the first. A
    /// `git clone` of a local checkout agrees on the root commit even though
    /// its `origin` is that checkout's path. An unrelated repository agrees
    /// on nothing, and a checkout with no commits matches no workspace and
    /// may move only into one that records no root commit either.
    #[test]
    fn a_workspace_holds_this_repository_when_any_clone_shares_its_root_commit() {
        let (home, _home_guard) = scratch_home("holds-repository");
        let parent = crate::scratch::root("holds-repository");
        let commit = |dir: &Path, message: &str| {
            run(
                dir,
                "git",
                &["commit", "--allow-empty", "-q", "-m", message],
            )
            .unwrap();
        };
        let first = parent.join("api");
        let unrelated = parent.join("other");
        let third = parent.join("third");
        let empty = parent.join("empty");
        for dir in [&first, &unrelated, &third, &empty] {
            std::fs::create_dir_all(dir).unwrap();
            crate::scratch::git_init(dir, &["-b", "main"]);
        }
        commit(&first, "root");
        commit(&unrelated, "another root");
        commit(&third, "a third root");
        let local = parent.join("api-review");
        run(
            &parent,
            "git",
            &[
                "clone",
                "-q",
                first.to_str().unwrap(),
                local.to_str().unwrap(),
            ],
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            // The unrelated repository is the workspace's first entry, and
            // the clone of `first` only its second.
            let created = create_workspace(&unrelated).unwrap();
            let name = created
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            join_workspace(&local, &name).unwrap();
            let summary = workspaces()
                .into_iter()
                .find(|workspace| workspace.name == name)
                .unwrap();

            let mine = root_commit(&first);
            assert!(
                summary.holds_repository_of(&first, mine.as_deref()),
                "the second entry's root commit counts"
            );
            assert!(summary.may_move_into(&first, mine.as_deref()));

            create_workspace(&third).unwrap();
            let only_unrelated = workspaces()
                .into_iter()
                .find(|workspace| workspace.name != name)
                .unwrap();
            assert!(
                !only_unrelated.holds_repository_of(&first, mine.as_deref()),
                "a workspace of another repository is not this one"
            );
            assert!(!only_unrelated.may_move_into(&first, mine.as_deref()));

            assert_eq!(root_commit(&empty), None);
            assert!(!summary.holds_repository_of(&empty, None));
            assert!(
                !summary.may_move_into(&empty, None),
                "a checkout with no root commit cannot move into a workspace that records one"
            );
        });
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// The mockup's own example: a path under home prints as `~/…`, and one
    /// outside it prints unchanged.
    #[test]
    fn shorten_home_rewrites_only_a_path_actually_under_it() {
        let home = crate::scratch::root("repo-test-shorten-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        crate::platform::test_home::with_home(&home, || {
            assert_eq!(
                shorten_home(&home.join("worktrees/spoolway/feature")),
                "~/worktrees/spoolway/feature"
            );
            assert_eq!(shorten_home(&home), "~");
            assert_eq!(
                shorten_home(Path::new("/elsewhere/project")),
                "/elsewhere/project"
            );
        });
    }

    /// `byproduct_dirs` is `retain`'s sweep's whole map of what it may
    /// delete — see its own doc. A composed system prompt is a byproduct;
    /// the project's own tracked prompt templates under `checkout` are not,
    /// and must never be swept.
    #[test]
    fn byproduct_dirs_names_the_composed_prompt_dir_not_the_tracked_templates() {
        let base = crate::scratch::root("repo-test-byproduct-dirs");
        let _ = std::fs::remove_dir_all(&base);
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config: crate::config::Config::default(),
            home: base.join(".home"),
        };

        let dirs = repo.byproduct_dirs();
        assert!(
            dirs.contains(&repo.system_prompts_dir()),
            "the sweep must reach composed system prompts"
        );
        assert!(
            !dirs.contains(&repo.prompts_dir()),
            "the sweep must never reach the checkout's own tracked prompt templates"
        );
        assert!(
            !dirs.contains(&repo.overrides_dir()),
            "the sweep must never reach the patch layer"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// The bug as reported: on a branch without `.spoolway/`, the ancestor
    /// walk climbed out of the checkout, found the global `~/.spoolway/`
    /// under `$HOME`, and called `$HOME` the project — so `queue add` wrote
    /// into `~/.spoolway/<user>/`. The global state root is never a
    /// project's `.spoolway`, and `~/.spoolway` itself is never a project.
    #[test]
    fn the_global_state_root_is_never_taken_for_a_project() {
        let (home, _home_guard) = scratch_home("global-state-root");
        let state_root = home.join(crate::config::STATE_DIR);
        // `~/.spoolway/.spoolway/` too, so the walk is tested against both
        // readings: `$HOME` as a root, and `~/.spoolway` as one.
        let inside = state_root.join(crate::config::STATE_DIR).join("deep");
        std::fs::create_dir_all(&inside).unwrap();

        // `Repo::root` rather than `Repo::discover`, because the walk is what
        // the bug was about and the walk is all this can assert everywhere.
        // On Windows the scratch root sits inside the real user profile, so an
        // ancestor above the scratch `$HOME` may carry a `.spoolway` of its
        // own and finding it is not this walk misbehaving.
        let found = crate::platform::test_home::with_home(&home, || Repo::root(&inside, None));
        match &found {
            // Nothing above carries a `.spoolway`: the walk fell all the way
            // through, which is the answer wherever the scratch tree is not
            // nested inside another project.
            Err(err) => {
                let said = format!("{err:#}");
                assert!(
                    said.contains("no spoolway project found"),
                    "the walk must fall through to the not-found error: {said}"
                );
            }
            // It stopped somewhere above. That is allowed — but never at
            // `$HOME` and never at `~/.spoolway`, which is the whole bug.
            Ok(root) => {
                assert_ne!(root, &home, "$HOME is never a project");
                assert_ne!(root, &state_root, "~/.spoolway is never a project");
            }
        }
        assert!(
            !state_root.join(home.file_name().unwrap()).exists(),
            "nothing may be created under the state root for a misread project"
        );
    }

    /// A checkout on a branch without `.spoolway/` must not find one above
    /// the repository — the walk is bounded at git's toplevel — and must not
    /// quietly fall back to the toplevel with a default config either, which
    /// is the other half of how a stray project came to exist.
    #[test]
    fn the_ancestor_walk_stops_at_the_git_toplevel() {
        let base = crate::scratch::root("repo-test-walk-bound");
        let _ = std::fs::remove_dir_all(&base);
        // A `.spoolway/` above the repository, where the old walk found it.
        std::fs::create_dir_all(base.join(crate::config::STATE_DIR)).unwrap();
        let work = base.join("work");
        let sub = work.join("src");
        std::fs::create_dir_all(&sub).unwrap();
        git(&work, &["init", "-q", "-b", "bare"]);

        let (home, _home_guard) = scratch_home("walk-bound");
        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&sub))
            .expect_err("a repository with no .spoolway/ is not a project");
        let said = format!("{err:#}");
        assert!(
            said.contains("no spoolway project found") && said.contains("spoolway init"),
            "{said}"
        );
        assert!(
            !home.join(crate::config::STATE_DIR).exists(),
            "no state directory may be created for a repository that is not a project"
        );
    }

    /// A project nobody has ever bound is not refused any more — the
    /// registration guard `claim`/`registered` used to insist on is gone,
    /// and this is acceptance criterion 7 of `binding-record`: nothing
    /// records this checkout anywhere, it carries no stamp, so it binds
    /// itself and proceeds.
    #[test]
    fn discovery_binds_a_project_nobody_has_bound_before() {
        let (_origin, work, _base_guard) = fixture("unbound");
        let (home, _home_guard) = scratch_home("unbound");

        let repo = crate::platform::test_home::with_home(&home, || Repo::discover(&work))
            .expect("a project with no home yet binds itself and proceeds");
        assert_eq!(repo.root.canonical().unwrap(), work.canonical().unwrap());
        assert!(
            repo.home.join(crate::repo::BINDING_FILE).is_file(),
            "the fresh binding is on disk"
        );
        assert!(
            std::fs::read_to_string(work.join(".git").join("spoolway-id")).is_ok(),
            "the checkout was stamped as part of binding itself"
        );
    }

    /// A plain git checkout, no `.spoolway/` and no commit needed — `bind`
    /// only ever reads and writes its own stamp and its home's record, so
    /// the lighter fixture the seven-state tests below share is enough.
    fn bind_fixture(name: &str) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(&format!("bind-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        root
    }

    /// Criterion 1: a valid stamp whose home already records this exact
    /// checkout proceeds, unchanged — the second of two calls changes
    /// nothing on disk.
    #[test]
    fn bind_criterion_1_an_already_correct_binding_proceeds_unchanged() {
        let work = bind_fixture("criterion-1");
        let (home, _home_guard) = scratch_home("criterion-1");
        crate::platform::test_home::with_home(&home, || {
            let first = bind(&work).unwrap();
            let record_before = std::fs::read_to_string(first.join(BINDING_FILE)).unwrap();
            let second = bind(&work).unwrap();
            assert_eq!(first, second);
            assert_eq!(
                std::fs::read_to_string(second.join(BINDING_FILE)).unwrap(),
                record_before,
                "an already-correct binding must not be rewritten"
            );
        });
    }

    /// Criterion 1's other half: the record naming this exact checkout is
    /// not enough on its own — the id it recorded has to agree with the
    /// stamp too, or a hand-edited stamp (or a hand-edited record) would
    /// proceed silently on a disagreement between the two files that must
    /// agree, exactly the failure the Goal names by name.
    #[test]
    fn bind_criterion_1_a_matching_root_but_a_disagreeing_id_refuses() {
        let work = bind_fixture("criterion-1b");
        let (home, _home_guard) = scratch_home("criterion-1b");
        let err = crate::platform::test_home::with_home(&home, || {
            let bound_home = bind(&work).unwrap();
            // The home's own directory name (and so the checkout's real
            // stamp) never changes here — only the `id` field inside
            // `project.toml`, hand-edited to something else. That is the
            // one way `binding.root == root` and `binding.id != id` can
            // happen at all: a home's directory is always named after the
            // id any *freshly written* record there carries, so only a
            // record tampered with after the fact can disagree with it.
            write_binding(
                &bound_home,
                &Binding {
                    id: "zzzzzz".to_string(),
                    root: work.canonical().unwrap(),
                },
            )
            .unwrap();
            bind(&work)
        })
        .expect_err("a root match with a disagreeing id must not proceed silently");
        let said = format!("{err:#}");
        assert!(said.contains("disagree about this checkout's id"), "{said}");
        assert!(said.contains(".git"), "names the stamp file: {said}");
        assert!(
            said.contains("project.toml"),
            "names the record file: {said}"
        );
        assert!(said.contains("delete"), "{said}");
    }

    /// Criterion 2: a home recording a checkout that is gone updates the
    /// record to this one instead of refusing over a claim nothing can
    /// still make.
    #[test]
    fn bind_criterion_2_a_home_recording_a_gone_checkout_moves_to_this_one() {
        let base = crate::scratch::root("bind-criterion-2");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let before = base.join("before");
        std::fs::create_dir_all(&before).unwrap();
        crate::scratch::git_init(&before, &["-b", "plan/demo"]);
        let (home, _home_guard) = scratch_home("criterion-2");

        crate::platform::test_home::with_home(&home, || {
            let bound_home = bind(&before).unwrap();
            let after = base.join("after");
            std::fs::rename(&before, &after).unwrap();

            let resolved = bind(&after).unwrap();
            assert_eq!(resolved, bound_home, "the same home, now updated");
            let binding: Binding =
                toml::from_str(&std::fs::read_to_string(bound_home.join(BINDING_FILE)).unwrap())
                    .unwrap();
            assert_eq!(binding.root, after.canonical().unwrap());
        });
    }

    /// Criterion 2, the other of its two causes: a home recording a
    /// checkout that still physically exists, but whose own stamp has
    /// since changed to something else — re-stamped by hand — moves to
    /// this one exactly as a gone checkout does. Not reachable through
    /// `rename` the way the first cause is, so this writes the disagreeing
    /// files directly.
    #[test]
    fn bind_criterion_2_the_other_cause_a_checkout_that_no_longer_carries_the_id_moves_to_this_one()
    {
        let base = crate::scratch::root("bind-criterion-2b");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (home, _home_guard) = scratch_home("criterion-2b");

        // The checkout the home's record still names — real, and still
        // where it was, but carrying a different id now.
        let old_checkout = base.join("old-checkout");
        std::fs::create_dir_all(&old_checkout).unwrap();
        crate::scratch::git_init(&old_checkout, &["-b", "plan/demo"]);

        // The checkout actually carrying the id the home is keyed on.
        let new_checkout = base.join("new-checkout");
        std::fs::create_dir_all(&new_checkout).unwrap();
        crate::scratch::git_init(&new_checkout, &["-b", "plan/demo"]);

        crate::platform::test_home::with_home(&home, || {
            stamp_over(&old_checkout, "aaaaaa").unwrap();
            stamp_over(&new_checkout, "bbbbbb").unwrap();
            let home_dir = crate::mux::project_home(&new_checkout).unwrap();
            write_binding(
                &home_dir,
                &Binding {
                    id: "bbbbbb".to_string(),
                    root: old_checkout.canonical().unwrap(),
                },
            )
            .unwrap();

            let resolved = bind(&new_checkout).unwrap();
            assert_eq!(resolved, home_dir);
            let binding: Binding =
                toml::from_str(&std::fs::read_to_string(home_dir.join(BINDING_FILE)).unwrap())
                    .unwrap();
            assert_eq!(binding.root, new_checkout.canonical().unwrap());
            assert_eq!(binding.id, "bbbbbb");
        });
    }

    /// Criterion 3: a home recording a checkout that still exists and
    /// still carries the same id refuses — two real checkouts sharing one
    /// id is a conflict `bind` cannot settle on its own.
    #[test]
    fn bind_criterion_3_two_checkouts_sharing_one_id_refuses() {
        let base = crate::scratch::root("bind-criterion-3");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let original = base.join("original");
        std::fs::create_dir_all(&original).unwrap();
        crate::scratch::git_init(&original, &["-b", "plan/demo"]);
        let (home, _home_guard) = scratch_home("criterion-3");

        let err = crate::platform::test_home::with_home(&home, || {
            bind(&original).unwrap();
            // A `cp -r` carries `.git` with it, so the copy stamps the same
            // id — the exact scenario the task's own mockup draws.
            let copy = base.join("copy");
            copy_dir(&original, &copy);
            bind(&copy)
        })
        .expect_err("two real checkouts must not both bind to the one home");
        let said = format!("{err:#}");
        assert!(said.contains("two checkouts carry the id"), "{said}");
        assert!(said.contains("delete"), "{said}");
    }

    /// A real failure reading the recorded checkout's own stamp — a
    /// permissions problem here, a corrupt repository in general — must
    /// refuse rather than being read the same as "no longer carries the
    /// id" and silently taken as licence to transfer the binding: neither
    /// proceeding nor guessing is allowed, only recording a move whose
    /// cause is actually known (see the task's non-goals).
    #[test]
    fn bind_an_indeterminate_read_of_the_other_checkout_refuses_rather_than_guessing() {
        use std::os::unix::fs::PermissionsExt;

        let base = crate::scratch::root("bind-indeterminate");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let original = base.join("original");
        std::fs::create_dir_all(&original).unwrap();
        crate::scratch::git_init(&original, &["-b", "plan/demo"]);
        let (home, _home_guard) = scratch_home("indeterminate");

        let bound_home = crate::platform::test_home::with_home(&home, || {
            let bound_home = bind(&original).unwrap();
            let copy = base.join("copy");
            copy_dir(&original, &copy);

            // `original`'s own stamp becomes unreadable, not merely absent
            // — a real I/O failure, distinct from every other cause `bind`
            // already tells apart.
            let stamp = original.join(".git").join("spoolway-id");
            let mut perms = std::fs::metadata(&stamp).unwrap().permissions();
            perms.set_mode(0o000);
            std::fs::set_permissions(&stamp, perms).unwrap();

            let err = bind(&copy).expect_err("an unreadable stamp must not be read as stale");
            let said = format!("{err:#}");
            assert!(said.contains("could not tell whether"), "{said}");
            assert!(said.contains(&original.display().to_string()), "{said}");

            // Restored before the rest of cleanup, so a failed assertion
            // above still leaves this directory removable.
            let mut perms = std::fs::metadata(&stamp).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&stamp, perms).unwrap();

            bound_home
        });

        // Untouched: the refusal must not have transferred the binding.
        let binding: Binding =
            toml::from_str(&std::fs::read_to_string(bound_home.join(BINDING_FILE)).unwrap())
                .unwrap();
        assert_eq!(binding.root, original.canonical().unwrap());
    }

    /// Criterion 4: a valid stamp, but no home recording it at all — the
    /// home was deleted, or nothing ever bound this checkout to it — must
    /// refuse, naming both files and how to resolve it by hand.
    #[test]
    fn bind_criterion_4_a_valid_stamp_with_no_home_refuses() {
        let work = bind_fixture("criterion-4");
        let (home, _home_guard) = scratch_home("criterion-4");
        let err = crate::platform::test_home::with_home(&home, || {
            // Stamped directly, bypassing `bind` — so a stamp exists but no
            // `project.toml` was ever written for it.
            stamped_id(&work).unwrap();
            bind(&work)
        })
        .expect_err("a stamped checkout with no home behind it must refuse");
        let said = format!("{err:#}");
        assert!(said.contains("no home holds the id"), "{said}");
        assert!(said.contains(".git"), "names the stamp file: {said}");
        assert!(
            said.contains("project.toml"),
            "names the record file: {said}"
        );
        assert!(said.contains("edit that home's own project.toml"), "{said}");
        assert!(said.contains("delete"), "{said}");
    }

    /// Criterion 5: a stamp that is not six lowercase base36 characters
    /// refuses, naming the format rather than treating it as unstamped.
    #[test]
    fn bind_criterion_5_a_malformed_stamp_refuses_naming_the_format() {
        let work = bind_fixture("criterion-5");
        let (home, _home_guard) = scratch_home("criterion-5");
        let git_dir = work.join(".git");
        std::fs::write(git_dir.join("spoolway-id"), "NOT-VALID!!\n").unwrap();

        let err = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect_err("a malformed stamp must be refused, not read as unstamped");
        let said = format!("{err:#}");
        assert!(
            said.contains("not six lowercase letters and digits"),
            "{said}"
        );
        assert!(said.contains("delete"), "{said}");
    }

    /// Criterion 6: no stamp, but some home already records this exact
    /// checkout — the stamp was deleted or never made it into this clone —
    /// refuses rather than silently minting a fresh id that home's record
    /// would then disagree with.
    #[test]
    fn bind_criterion_6_no_stamp_where_a_home_already_records_this_path_refuses() {
        let work = bind_fixture("criterion-6");
        let (home, _home_guard) = scratch_home("criterion-6");
        let err = crate::platform::test_home::with_home(&home, || {
            bind(&work).unwrap();
            std::fs::remove_file(work.join(".git").join("spoolway-id")).unwrap();
            bind(&work)
        })
        .expect_err("a home already records this checkout by path");
        let said = format!("{err:#}");
        assert!(said.contains("no id"), "{said}");
        assert!(
            said.contains("project.toml"),
            "names the record file: {said}"
        );
        assert!(said.contains("restore the stamp"), "{said}");
    }

    /// Criterion 7: no stamp, and nothing records this checkout anywhere —
    /// there is nothing to guess, so it binds itself and proceeds. Already
    /// exercised through `Repo::discover` by
    /// `discovery_binds_a_project_nobody_has_bound_before`; this is the
    /// same fact at `bind`'s own level.
    #[test]
    fn bind_criterion_7_nothing_recorded_anywhere_binds_itself() {
        let work = bind_fixture("criterion-7");
        let (home, _home_guard) = scratch_home("criterion-7");
        let bound = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("nothing recorded anywhere binds itself and proceeds");
        assert!(bound.join(BINDING_FILE).is_file());
        assert!(work.join(".git").join("spoolway-id").is_file());
    }

    /// `all_workspaces` bails outright on the first workspace `project.toml`
    /// it cannot parse, and `bind` asks it before checking whether `root` is
    /// even listed anywhere — so one unrelated workspace file broken by a
    /// typo stops every checkout on the machine from binding, this one
    /// included, though nothing about it names the broken workspace at all.
    /// Wanted instead: a checkout nothing points to binds itself exactly as
    /// criterion 7 does, and the broken file becomes one note elsewhere, not
    /// a hard stop here.
    #[test]
    fn bind_skips_an_unreadable_workspace_file_for_a_checkout_it_does_not_list() {
        let work = bind_fixture("skips-unreadable");
        let (home, _home_guard) = scratch_home("skips-unreadable");
        let broken = home.join(".spoolway").join("a-1x");
        std::fs::create_dir_all(broken.join("config")).unwrap();
        std::fs::write(broken.join(BINDING_FILE), "clones = [\n").unwrap();

        let bound = crate::platform::test_home::with_home(&home, || bind(&work));
        assert!(
            bound.is_ok(),
            "a checkout listed nowhere must not be stopped by an unrelated \
             workspace's unreadable project.toml: {:?}",
            bound.err()
        );
    }

    /// The other half of the same fix: a checkout that really is in no
    /// *readable* workspace must still refuse while any workspace file is
    /// unreadable, rather than quietly binding itself the way criterion 7
    /// does — the broken file might be exactly the one that would have
    /// named it, so answering "nothing lists this" here would be a guess.
    /// `Repo::root`'s own last-resort scan is what must say so, never the
    /// generic "no spoolway project found … run `spoolway init`", which
    /// would convert a checkout that might be a listed clone to repo mode.
    #[test]
    fn root_refuses_a_checkout_found_in_no_readable_workspace_while_one_is_unreadable() {
        let work = bind_fixture("root-refuses-unreadable");
        let (home, _home_guard) = scratch_home("root-refuses-unreadable");
        let broken = home.join(".spoolway").join("a-1x");
        std::fs::create_dir_all(broken.join("config")).unwrap();
        std::fs::write(broken.join(BINDING_FILE), "clones = [\n").unwrap();

        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&work))
            .expect_err("a checkout that might be listed in the broken file must refuse");
        let err = format!("{err:#}");
        assert!(
            err.contains(&broken.join(BINDING_FILE).display().to_string()),
            "names the unreadable file: {err}"
        );
        assert!(
            !err.contains("no spoolway project found"),
            "must not fall through to the generic not-found message, which tells the person to \
             run `spoolway init` and so converts this checkout to repo mode: {err}",
        );
    }

    /// A clone a readable workspace still lists, but whose shared `config/`
    /// has been lost, must refuse naming the missing folder — not the
    /// generic "no spoolway project found", which `spoolway init --yes`
    /// would answer by writing a fresh default config into the very folder
    /// every other clone of that workspace already shares.
    #[test]
    fn root_refuses_a_listed_clone_whose_workspace_has_no_config() {
        let work = bind_fixture("listed-no-config");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("listed-no-config", &canon, "api");
        std::fs::remove_dir_all(
            home.join(".spoolway")
                .join("listed-no-config-ws")
                .join("config"),
        )
        .unwrap();

        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&work))
            .expect_err("a listed clone with no shared config/ must refuse");
        let err = format!("{err:#}");
        let missing = home
            .join(".spoolway")
            .join("listed-no-config-ws")
            .join("config");
        assert!(
            err.contains(&missing.display().to_string()),
            "names the missing config/ folder: {err}"
        );
        assert!(
            !err.contains("no spoolway project found"),
            "must not read as an unconfigured checkout: {err}",
        );
    }

    /// [`process_cwd_under`]'s own primitive, against a real child process
    /// with a real cwd — not a fake one recorded in a headless lane's own
    /// JSON, and not this test process's own cwd (elsewhere, not under
    /// `dir`), so a false positive here would mean the `/proc` scan itself
    /// is reading the wrong thing rather than the fixture accidentally
    /// matching.
    #[cfg(target_os = "linux")]
    #[test]
    fn process_cwd_under_finds_a_real_process_whose_cwd_is_inside_it() {
        let dir = crate::scratch::root("process-cwd-under");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let elsewhere = crate::scratch::root("process-cwd-under-elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();

        assert!(!process_cwd_under(&dir), "nothing is running there yet");

        let mut child = std::process::Command::new("sleep")
            .arg("20")
            .current_dir(&dir)
            .spawn()
            .expect("`sleep` is on PATH in this environment");

        assert!(
            process_cwd_under(&dir),
            "a real, live process's cwd is inside `dir`"
        );
        assert!(
            !process_cwd_under(&elsewhere),
            "the same process's cwd is not inside an unrelated directory"
        );

        child.kill().unwrap();
        let _ = child.wait();
        assert!(
            !process_cwd_under(&dir),
            "a killed process no longer counts, whatever `/proc` briefly still shows"
        );
    }

    /// A plain recursive copy, `cp -r`'s own behaviour: every file under
    /// `from`, `.git` included, landing at the same relative path under
    /// `to`. Used only to build the "two checkouts, one id" fixture
    /// criterion 3 needs — a real `cp -r`, not a fresh `git clone`, is what
    /// carries the untracked `.git/spoolway-id` file along with it.
    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let dest = to.join(entry.file_name());
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                copy_dir(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), &dest).unwrap();
            }
        }
    }

    /// `.spoolway/` is tracked, so a checkout is a project on one branch
    /// and not on another. A bound project on a branch that lacks it
    /// is told exactly that, by branch name, rather than the generic
    /// not-found error a never-initialised directory gets.
    #[test]
    fn a_bound_project_on_a_branch_without_its_state_dir_is_told_so() {
        let (_origin, work, _base_guard) = fixture("branch-without-state");
        let (home, _home_guard) = scratch_home("branch-without-state");
        let err = crate::platform::test_home::with_home(&home, || {
            Repo::discover(&work).expect("binds on the branch that still carries .spoolway/");
            git(&work, &["checkout", "-q", "-b", "bare"]);
            git(&work, &["rm", "-q", "-r", crate::config::STATE_DIR]);
            git(&work, &["commit", "-q", "-m", "drop the control plane"]);
            Repo::discover(&work)
        })
        .expect_err("no .spoolway/ on this branch");
        let said = format!("{err:#}");
        assert!(
            said.contains("is a spoolway project, but `.spoolway/` is not on branch `bare`"),
            "{said}"
        );
        assert!(
            said.contains("check out a branch that carries it"),
            "the error says what to do: {said}"
        );
    }

    /// The shape `project_home` keys a home directory name on: short enough
    /// to read in a path, and drawn from a fixed alphabet so a directory
    /// listing can pick a stamped id back out of `<label>-<id>` unambiguously.
    #[test]
    fn stamped_id_is_six_lowercase_alphanumeric_characters() {
        let (_origin, work, _base_guard) = fixture("stamp-format");
        let (id, minted) = stamped_id(&work)
            .unwrap()
            .expect("a real git repo stamps an id");
        assert!(
            minted,
            "a fresh repo's first stamp is the one that mints it"
        );
        assert_eq!(id.len(), 6, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "{id}"
        );
    }

    /// Read back, not re-minted: a second call must not hand a project a new
    /// home every time something asks for one.
    #[test]
    fn stamped_id_is_stable_across_calls() {
        let (_origin, work, _base_guard) = fixture("stamp-stable");
        let (first, first_minted) = stamped_id(&work).unwrap().unwrap();
        let (second, second_minted) = stamped_id(&work).unwrap().unwrap();
        assert_eq!(first, second, "the id is read back, not re-minted");
        assert!(first_minted, "the first call is the one that mints it");
        assert!(!second_minted, "the second call only reads it back");
    }

    /// Written where the task's own mockup draws it: `.git/spoolway-id` in
    /// the ordinary, non-worktree case.
    #[test]
    fn stamped_id_is_written_into_the_common_git_directory() {
        let (_origin, work, _base_guard) = fixture("stamp-file");
        let (id, _minted) = stamped_id(&work).unwrap().unwrap();
        let on_disk = std::fs::read_to_string(work.join(".git").join("spoolway-id")).unwrap();
        assert_eq!(on_disk.trim(), id);
    }

    /// A bare fixture directory with no git repository behind it at all has
    /// nowhere to write a stamp, and nowhere is exactly the answer this
    /// gives — never a made-up id that would tempt a caller into a home it
    /// has no business creating. `Ok(None)`, not an error: this is an
    /// ordinary, expected fact about the directory, not something having
    /// gone wrong.
    #[test]
    fn a_directory_with_no_git_repository_has_no_stamped_id() {
        let base = crate::scratch::root("repo-test-no-git");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(stamped_id(&base).unwrap().is_none());
    }

    /// A real git repository whose stamp cannot be written — the common git
    /// directory itself is read-only, standing in for a permissions problem
    /// or a full disk — must not read the same as "no git repository at
    /// all": that would send `spoolway init` to refuse for a reason that
    /// is not true, and a caller resolving a home would silently fall back
    /// to a basename-keyed one instead of reporting that something is
    /// actually wrong.
    #[test]
    fn a_write_failure_is_an_error_not_an_absent_stamp() {
        use std::os::unix::fs::PermissionsExt;
        let (_origin, work, _base_guard) = fixture("stamp-write-failure");
        let git_dir = work.join(".git");
        let mut perms = std::fs::metadata(&git_dir).unwrap().permissions();
        perms.set_mode(0o500); // read + execute, no write
        std::fs::set_permissions(&git_dir, perms.clone()).unwrap();

        let err = stamped_id(&work);

        // Restore before asserting, so a failed assertion does not leave a
        // directory this test's own cleanup cannot remove.
        perms.set_mode(0o700);
        std::fs::set_permissions(&git_dir, perms).unwrap();

        assert!(
            err.is_err(),
            "a write that fails for a real reason must not read as no repository: {err:?}"
        );
    }

    /// Two processes resolving one project's home for the first time at
    /// once must still agree on one id — the write that lands second reads
    /// the first one's answer back rather than minting a competing one.
    #[test]
    fn concurrent_first_resolvers_agree_on_one_id() {
        let (_origin, work, _base_guard) = fixture("stamp-race");
        let ids: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| stamped_id(&work).unwrap().unwrap().0))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let first = &ids[0];
        assert!(
            ids.iter().all(|id| id == first),
            "every racing resolver must land on the same id: {ids:?}"
        );
    }

    /// Two clones of one repository never share a `.git`, so two calls to
    /// this mint two different ids — the whole reason a home is keyed off
    /// one, rather than off the basename the two clones might well share.
    /// Real clones of one origin, not two independently `git init`ed
    /// directories that only happen to look alike.
    #[test]
    fn two_clones_of_one_repository_stamp_two_different_ids() {
        let (origin, _work, _base_guard) = fixture("stamp-clone-origin");
        let base = origin.parent().unwrap();

        let clone_a = base.join("clone-a");
        let clone_b = base.join("clone-b");
        for clone in [&clone_a, &clone_b] {
            git(
                base,
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
        }

        assert_ne!(
            stamped_id(&clone_a).unwrap().unwrap().0,
            stamped_id(&clone_b).unwrap().unwrap().0,
            "two clones of the same origin must not share a home"
        );
    }

    /// The label frozen at stamp time survives even when nothing has ever
    /// created `~/.spoolway/<label>-<id>/` on disk — the id and the label
    /// it was minted alongside both live in the checkout's own `.git`, never
    /// recovered by guessing from a directory listing that may not exist
    /// yet.
    #[test]
    fn project_identity_freezes_the_label_at_first_stamp() {
        let work = crate::scratch::root("repo-test-identity-label");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        crate::scratch::git_init(&work, &["-q", "-b", "main"]);
        stamped_id(&work).unwrap(); // stands in for `spoolway init`

        let (checkout, label, id) = project_identity(&work).unwrap().unwrap();
        assert_eq!(checkout, work.canonical().unwrap());
        assert_eq!(label, crate::mux::project_label(&work));

        // Renamed with nothing under `~/.spoolway/` ever created for it.
        let renamed = crate::scratch::root("repo-test-identity-label-renamed");
        let _ = std::fs::remove_dir_all(&renamed);
        std::fs::rename(&work, &renamed).unwrap();

        let (_checkout, label_after, id_after) = project_identity(&renamed).unwrap().unwrap();
        assert_eq!(label_after, label, "the label must not follow the rename");
        assert_eq!(id_after, id);
    }

    /// A corrupted `spoolway-label` — a person's own edit, a stray write
    /// from something else entirely — must never be trusted as one path
    /// component to `join` onto `~/.spoolway/` unchecked: that is how a
    /// value like `/tmp/victim` or `../../victim` would walk `project_home`
    /// outside its own state root altogether. Read back as no identity at
    /// all — the same as an unstamped project — rather than as a value that
    /// could escape it: `project_identity` only ever peeks, so it has no
    /// valid label of its own to fall back to and mint here.
    #[test]
    fn a_label_that_could_leave_the_state_root_is_never_read_back() {
        let (_origin, work, _base_guard) = fixture("label-traversal");
        stamped_id(&work).unwrap();
        let label_path = work.join(".git").join("spoolway-label");

        for escape in ["/tmp/victim", "../../victim", "..", ".", "a/b", "a\\b"] {
            std::fs::write(&label_path, escape).unwrap();
            assert!(
                project_identity(&work).unwrap().is_none(),
                "`{escape}` was accepted as a safe label"
            );
        }
    }

    /// A checkout basename that is not itself a safe label — `\` is a
    /// perfectly ordinary byte in a Unix filename, but `is_valid_label`
    /// (the same rule a hook name is held to) refuses it as a path
    /// separator — must still come out of `stamped_id` with an identity
    /// `project_identity` can read straight back.
    ///
    /// `read_or_mint` used to write whatever `mint()` returned unchecked:
    /// the committed `spoolway-label` failed `is_valid_label` on the very
    /// next read, so `project_identity` treated the project as never
    /// stamped and every caller fell back to the basename — even though a
    /// file calling itself "the" stamp sat right there in `.git`.
    ///
    /// A raw `\` is a legal Unix filename character and no path separator
    /// at all, so `.join("api\\copy")` below names one unsafe basename
    /// rather than two path components.
    /// `sanitize_label_folds_an_unsafe_character_to_a_safe_one` below covers
    /// the same fix straight against `sanitize_label` rather than through a
    /// real rename.
    #[test]
    fn a_basename_that_is_not_a_safe_label_still_stamps_and_reads_back() {
        let base = crate::scratch::root("repo-test-unsafe-label");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        crate::scratch::git_init(&base, &["-q", "-b", "main"]);
        // Renamed to a basename `is_valid_label` refuses outright — a raw
        // `\` is legal in a Unix filename but is not one path component to
        // `is_bare_filename`, which treats it the same as `/`.
        //
        // The new name is the root's own basename with `-api\copy` appended,
        // not a bare `api\copy` beside it. A bare name sits directly in the
        // shared temporary directory, where every concurrent `cargo test`
        // process picks the identical path — one run's `remove_dir_all` then
        // races the other's `rename`, which fails `ENOTEMPTY`. Appending to
        // the root keeps the pid and sequence number that make the path this
        // process's alone, and `scratch::finished_run_pid` still reclaims it,
        // since it allows exactly this trailing word.
        let weird = base.with_file_name(format!(
            "{}-api\\copy",
            base.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&weird);
        std::fs::rename(&base, &weird).unwrap();

        let (id, minted) = stamped_id(&weird).unwrap().unwrap();
        assert!(minted, "the first call must be the one that mints");

        let (_checkout, label, id_again) = project_identity(&weird)
            .unwrap()
            .unwrap_or_else(|| panic!("an unsafe basename must still resolve to a real identity"));
        assert_eq!(id_again, id);
        assert!(
            is_valid_label(&label),
            "the committed label {label:?} must be safe to join onto the state root"
        );
    }

    /// [`sanitize_label`] directly, on every platform: a basename already
    /// safe is returned unchanged, and one that is not (a `/` or `\`, the
    /// two separators either platform could hand it) comes back safe
    /// without losing the rest of the name.
    #[test]
    fn sanitize_label_folds_an_unsafe_character_to_a_safe_one() {
        assert_eq!(sanitize_label("api"), "api");
        for unsafe_basename in ["api\\copy", "a/b"] {
            let sanitized = sanitize_label(unsafe_basename);
            assert!(
                is_valid_label(&sanitized),
                "{unsafe_basename:?} sanitized to {sanitized:?}, still not a safe label"
            );
            assert_ne!(sanitized, unsafe_basename);
        }
    }

    /// A scratch `$HOME` carrying a hand-written workspace — `project.toml`
    /// naming `clone` as `dispatcher`, plus `config/` with an empty
    /// `config.toml` — the fixture every `home-mode-discovery` test shares.
    /// Written by hand rather than through `spoolway init`'s own
    /// `create_workspace`, so these tests pin discovery alone.
    fn workspace_fixture(
        name: &str,
        clone: &Path,
        dispatcher: &str,
    ) -> (PathBuf, crate::scratch::ScratchRoot) {
        let (home, home_guard) = scratch_home(name);
        let workspace = home.join(".spoolway").join(format!("{name}-ws"));
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(workspace.join("config").join("config.toml"), "").unwrap();
        std::fs::write(
            workspace.join(BINDING_FILE),
            format!(
                "id = \"{name}\"\nclones = [{{ root = {:?}, dispatcher = {dispatcher:?} }}]\n",
                clone.display(),
            ),
        )
        .unwrap();
        (home, home_guard)
    }

    /// A checkout with no `.spoolway/` and no stamp, listed by a workspace's
    /// `project.toml`, resolves through it: the setup accessor reads the
    /// workspace's `config/`, and `Repo::home` is the dispatcher folder the
    /// workspace itself names — not a stamp-keyed home anywhere else.
    #[test]
    fn home_mode_checkout_resolves_through_the_workspace() {
        let work = bind_fixture("home-mode-basic");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-basic", &canon, "api");

        crate::platform::test_home::with_home(&home, || {
            let repo = Repo::discover(&work)
                .expect("a checkout a workspace lists by path is a working project");
            assert_eq!(
                repo.checkout, canon,
                "no branch-local setup to be checkout-specific about"
            );
            assert_eq!(
                repo.home,
                home.join(".spoolway")
                    .join("home-mode-basic-ws")
                    .join("dispatchers")
                    .join("api"),
                "home is the workspace's own dispatcher folder, not a stamp-keyed one"
            );
            assert_eq!(
                repo.setup_dir(),
                home.join(".spoolway")
                    .join("home-mode-basic-ws")
                    .join("config"),
                "the setup accessor reads the workspace's config/"
            );
        });
        assert!(
            std::fs::read_to_string(canon.join(".git").join("spoolway-id")).is_err(),
            "home mode must write nothing into .git"
        );
    }

    /// `all_workspaces` drops a `project.toml` that fails to parse
    /// (`toml::from_str(..).ok()?`) with no message at all, so a workspace
    /// broken by a stray edit or a bad permission simply stops being found.
    /// `Repo::discover` then falls through to the generic "no spoolway
    /// project found … run `spoolway init`" — which, followed, converts the
    /// clone to repo mode and stamps `.git`, exactly the silent-fallback
    /// `workspace-scan-strict` task describes. The fix must make this name
    /// the broken `project.toml` instead.
    #[test]
    fn a_broken_workspace_file_is_reported_instead_of_silently_skipped() {
        let work = bind_fixture("home-mode-broken-toml");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-broken-toml", &canon, "api");
        let project_toml = home
            .join(".spoolway")
            .join("home-mode-broken-toml-ws")
            .join(BINDING_FILE);
        let mut broken = std::fs::read_to_string(&project_toml).unwrap();
        broken.push_str("garbage = [\n");
        std::fs::write(&project_toml, broken).unwrap();

        crate::platform::test_home::with_home(&home, || {
            let err = Repo::discover(&work)
                .expect_err("a workspace file that fails to parse must be reported, not skipped")
                .to_string();
            assert!(
                err.contains(&project_toml.display().to_string()),
                "error must name the broken file {}, got: {err}",
                project_toml.display(),
            );
            assert!(
                !err.contains("no spoolway project found"),
                "must not fall through to the generic not-found message, which tells the \
                 person to run `spoolway init` and so converts this clone to repo mode: {err}",
            );
        });
    }

    /// Everything a 0.6.0 install (or an older, legacy one) could have left
    /// under `~/.spoolway/` beside a real workspace: another 0.6.0
    /// repo-mode home (an ordinary [`Binding`] — `id` and `root`, no
    /// `clones` — with none of a workspace's `config/`/`dispatchers/`
    /// folders beside it), a legacy home with no `project.toml` at all, and
    /// a folder that is no home at all. Shared by the home-mode and
    /// repo-mode variants of acceptance criterion 6's test, below.
    fn seed_0_6_0_siblings(state: &Path) {
        let repo_mode = state.join("repo-mode-home-abc123");
        std::fs::create_dir_all(&repo_mode).unwrap();
        std::fs::write(
            repo_mode.join(BINDING_FILE),
            "id = \"abc123\"\nroot = \"/some/other/checkout\"\n",
        )
        .unwrap();

        let legacy = state.join("spoolway");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("lanes.json"), "[]").unwrap();

        std::fs::create_dir_all(state.join("logs")).unwrap();
    }

    /// Acceptance criterion 6, home-mode case: a `~/.spoolway/` mixing a
    /// real workspace with everything a 0.6.0 (or legacy) install could
    /// have left beside it produces no new error or warning. `all_workspaces`
    /// tells these apart from a workspace by shape — a `clones` key, or a
    /// `config/`/`dispatchers/` folder beside `project.toml` — not merely by
    /// failing to parse, so this pins that every one of them is still
    /// skipped silently rather than now being mistaken for a broken
    /// workspace.
    #[test]
    fn a_mixed_home_from_older_installs_produces_no_new_error() {
        let work = bind_fixture("home-mode-mixed");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-mixed", &canon, "api");
        let state = home.join(".spoolway");
        seed_0_6_0_siblings(&state);

        crate::platform::test_home::with_home(&home, || {
            let repo = Repo::discover(&work)
                .expect("the real workspace still resolves past everything beside it");
            assert_eq!(
                repo.setup_dir(),
                state.join("home-mode-mixed-ws").join("config"),
                "resolution is unaffected by the repo-mode home, the legacy home, or logs/"
            );
        });
    }

    /// Acceptance criterion 6, repo-mode case: the same mixed `~/.spoolway/`
    /// must not trip up a checkout that is itself in repo mode either —
    /// `bind` scans every workspace under `~/.spoolway/` for every ordinary
    /// command, home mode or not, so a repo-mode project is exactly as
    /// exposed to a stray 0.6.0 sibling as a home-mode one.
    #[test]
    fn a_mixed_home_from_older_installs_does_not_affect_a_repo_mode_project() {
        let (_origin, work, _base_guard) = fixture("repo-mode-mixed");
        let (home, _home_guard) = scratch_home("repo-mode-mixed");
        seed_0_6_0_siblings(&home.join(".spoolway"));

        crate::platform::test_home::with_home(&home, || {
            let repo = Repo::discover(&work)
                .expect("a repo-mode project resolves past everything under ~/.spoolway/");
            assert_eq!(
                repo.setup_dir(),
                work.join(crate::config::STATE_DIR),
                "repo mode reads its own tracked .spoolway/, unaffected by any of it"
            );
        });
    }

    /// Acceptance criterion 3, same-workspace case: one `project.toml`
    /// listing `root` twice — two `clones` entries, two dispatcher folders —
    /// is an error naming both entries rather than silently binding to
    /// whichever `clones[]` happens to be found first.
    #[test]
    fn a_checkout_listed_twice_in_one_workspace_is_refused() {
        let work = bind_fixture("home-mode-dup-same-ws");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-dup-same-ws", &canon, "api");
        let record = home
            .join(".spoolway")
            .join("home-mode-dup-same-ws-ws")
            .join(BINDING_FILE);
        std::fs::write(
            &record,
            format!(
                "id = \"home-mode-dup-same-ws\"\nclones = [{{ root = {0:?}, dispatcher = \
                 \"api\" }}, {{ root = {0:?}, dispatcher = \"api-2\" }}]\n",
                canon.display(),
            ),
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let err = Repo::discover(&work)
                .expect_err("a checkout listed twice in one workspace must refuse")
                .to_string();
            assert!(
                err.contains(&canon.display().to_string()),
                "error must name the checkout, got: {err}"
            );
            assert!(
                err.contains("api") && err.contains("api-2"),
                "error must name both dispatcher entries, got: {err}"
            );
            assert!(
                err.contains(&record.display().to_string()),
                "error must name the file carrying both entries, got: {err}"
            );
        });
    }

    /// Acceptance criterion 3, cross-workspace case: two different
    /// workspaces each listing the same checkout is just as much a
    /// disagreement as one workspace listing it twice, so it refuses the
    /// same way, naming both `project.toml` files.
    #[test]
    fn a_checkout_listed_by_two_workspaces_is_refused() {
        let work = bind_fixture("home-mode-dup-cross-ws");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-dup-cross-ws", &canon, "api");
        let first_record = home
            .join(".spoolway")
            .join("home-mode-dup-cross-ws-ws")
            .join(BINDING_FILE);

        let second = home.join(".spoolway").join("home-mode-dup-cross-ws-ws-2");
        std::fs::create_dir_all(second.join("config")).unwrap();
        let second_record = second.join(BINDING_FILE);
        std::fs::write(
            &second_record,
            format!(
                "id = \"home-mode-dup-cross-ws-2\"\nclones = [{{ root = {:?}, dispatcher = \
                 \"api\" }}]\n",
                canon.display(),
            ),
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let err = Repo::discover(&work)
                .expect_err("a checkout two workspaces both list must refuse")
                .to_string();
            assert!(
                err.contains(&first_record.display().to_string())
                    && err.contains(&second_record.display().to_string()),
                "error must name both workspaces' project.toml, got: {err}"
            );
        });
    }

    /// Acceptance criterion 4, the dispatcher half: a hand-edited
    /// `dispatcher = "../../../ESCAPED"` is refused by every reader rather
    /// than joined straight onto the workspace folder, which is how a
    /// command used to end up creating `$HOME/ESCAPED/`.
    #[test]
    fn a_dispatcher_that_is_not_one_plain_name_is_refused() {
        let work = bind_fixture("home-mode-dispatcher-escape");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-dispatcher-escape", &canon, "api");
        let record = home
            .join(".spoolway")
            .join("home-mode-dispatcher-escape-ws")
            .join(BINDING_FILE);
        std::fs::write(
            &record,
            format!(
                "id = \"home-mode-dispatcher-escape\"\nclones = [{{ root = {:?}, dispatcher = \
                 \"../../../ESCAPED\" }}]\n",
                canon.display(),
            ),
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let err = Repo::discover(&work)
                .expect_err("a dispatcher carrying a path separator or `..` must be refused")
                .to_string();
            assert!(
                err.contains("ESCAPED"),
                "error must name the bad dispatcher value, got: {err}"
            );
            assert!(
                err.contains(&record.display().to_string()),
                "error must name the file, got: {err}"
            );
        });
        assert!(
            !home.join("ESCAPED").exists(),
            "must never create a folder outside the workspace"
        );
    }

    /// Acceptance criterion 4, the UTF-8 half: `create_workspace`,
    /// `join_workspace` and `move_clone` all refuse a `root` with bytes that
    /// are not valid UTF-8 rather than let it reach `write_workspace`, where
    /// it could only be recorded lossily (or fail with `toml`'s own, less
    /// specific, serialization error).
    #[cfg(unix)]
    #[test]
    fn a_clone_path_that_is_not_valid_utf8_is_refused_rather_than_stored_lossily() {
        use std::os::unix::ffi::OsStringExt;

        let parent = crate::scratch::root("init-home-non-utf8");
        std::fs::create_dir_all(&parent).unwrap();
        let mut bytes = parent.as_os_str().to_os_string().into_vec();
        bytes.extend_from_slice(b"/bad-\xff-name");
        let root = PathBuf::from(std::ffi::OsString::from_vec(bytes));

        let (home, _home_guard) = scratch_home("home-mode-non-utf8");
        crate::platform::test_home::with_home(&home, || {
            let messages = [
                create_workspace(&root).err().map(|e| e.to_string()),
                join_workspace(&root, "whatever")
                    .err()
                    .map(|e| e.to_string()),
                move_clone(&root, "whatever", None)
                    .err()
                    .map(|e| e.to_string()),
            ];
            for message in messages {
                let message = message.expect("each writer must refuse rather than succeed");
                assert!(
                    message.contains("UTF-8"),
                    "error must say the path is not valid UTF-8, got: {message}"
                );
            }
        });
    }

    /// Bug: `init` took its root from [`run`], whose `from_utf8_lossy` turned
    /// a `0xFF` byte into U+FFFD before [`require_utf8_root`] saw it, so the
    /// lossy spelling was stored and never matched again. [`toplevel_raw`],
    /// what `init` now resolves its root with, keeps git's bytes as they are.
    #[cfg(unix)]
    #[test]
    fn init_resolves_a_non_utf8_checkout_losslessly_so_it_is_refused() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let parent = crate::scratch::root("init-toplevel-non-utf8");
        std::fs::create_dir_all(&parent).unwrap();
        let mut bytes = parent.as_os_str().to_os_string().into_vec();
        bytes.extend_from_slice(b"/bad-\xff-name");
        let checkout = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q"]);

        let root = toplevel_raw(&checkout).unwrap();
        assert!(
            root.as_os_str().as_bytes().ends_with(b"/bad-\xff-name"),
            "the toplevel must keep git's raw bytes, got {root:?}"
        );

        let (home, _home_guard) = scratch_home("init-toplevel-non-utf8");
        crate::platform::test_home::with_home(&home, || {
            let message = create_workspace(&root)
                .expect_err("a non-UTF-8 root must be refused, not stored lossily")
                .to_string();
            assert!(message.contains("UTF-8"), "got: {message}");
        });
    }

    /// The same checkout, asked about from one of its own linked worktrees —
    /// a lane's own worktree carries no `.spoolway/` either, and there is no
    /// branch-local workspace config to be local about, so it resolves
    /// exactly as the main checkout does.
    #[test]
    fn home_mode_resolves_from_a_linked_worktree() {
        let work = bind_fixture("home-mode-worktree");
        let canon = work.canonical().unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-worktree", &canon, "api");
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("README"), "hello\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "root"]);
        let lane = crate::scratch::root("home-mode-worktree-lane");
        let _ = std::fs::remove_dir_all(&lane);
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/x",
                lane.to_str().unwrap(),
            ],
        );

        crate::platform::test_home::with_home(&home, || {
            let repo = Repo::discover(&lane)
                .expect("a linked worktree of a home-mode clone resolves the same way");
            assert_eq!(repo.root, canon);
            assert_eq!(
                repo.setup_dir(),
                home.join(".spoolway")
                    .join("home-mode-worktree-ws")
                    .join("config")
            );
        });
    }

    /// A checkout carrying a tracked `.spoolway/` *and* listed by a
    /// workspace is ambiguous — refused, naming both the tracked directory
    /// and the workspace's own record.
    #[test]
    fn home_mode_refuses_a_checkout_that_also_carries_a_tracked_spoolway() {
        let work = bind_fixture("home-mode-conflict");
        let canon = work.canonical().unwrap();
        std::fs::create_dir_all(work.join(crate::config::STATE_DIR)).unwrap();
        std::fs::write(work.join(crate::config::STATE_DIR).join("config.toml"), "").unwrap();
        let (home, _home_guard) = workspace_fixture("home-mode-conflict", &canon, "api");

        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&work))
            .expect_err("a checkout cannot be both a tracked project and a workspace clone");
        let said = format!("{err:#}");
        assert!(said.contains(".spoolway"), "{said}");
        assert!(said.contains(BINDING_FILE), "{said}");
    }

    /// Nothing here names the checkout being asked about, but a workspace
    /// elsewhere lists a clone whose folder is gone, and the checkout being
    /// asked about shares that clone's root commit — exactly the case
    /// [`join_workspace`] would take its queue over for. The "no spoolway
    /// project found" error names that workspace, with the exact,
    /// shell-quoted `init --workspace` line that takes it over, and the old
    /// path that clone used to be at.
    #[test]
    fn no_project_found_lists_a_stale_clone_with_its_workspace_line() {
        let missing = crate::scratch::root("home-mode-stale-gone");
        std::fs::create_dir_all(&missing).unwrap();
        crate::scratch::git_init(&missing, &["-b", "main"]);
        run(
            &missing,
            "git",
            &["commit", "--allow-empty", "-q", "-m", "root"],
        )
        .unwrap();
        let commit = root_commit(&missing).expect("a real commit has a root commit");

        // A clone of the same repository, taken before `missing` is removed
        // — the checkout that replaces it, sharing its root commit.
        let elsewhere = crate::scratch::root("home-mode-stale-elsewhere");
        let _ = std::fs::remove_dir_all(&elsewhere);
        run(
            missing.parent().unwrap(),
            "git",
            &[
                "clone",
                "-q",
                missing.to_str().unwrap(),
                elsewhere.to_str().unwrap(),
            ],
        )
        .unwrap();

        std::fs::remove_dir_all(&missing).unwrap();

        let (home, _home_guard) = scratch_home("home-mode-stale");
        let workspace = home.join(".spoolway").join("home-mode-stale-ws");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(workspace.join("config").join("config.toml"), "").unwrap();
        std::fs::write(
            workspace.join(BINDING_FILE),
            format!(
                "id = \"home-mode-stale\"\nclones = [{{ root = {:?}, dispatcher = \"api\", \
                 root_commit = {commit:?} }}]\n",
                missing.display(),
            ),
        )
        .unwrap();

        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&elsewhere))
            .expect_err("the clone of the same repository is still not a project itself");
        let said = format!("{err:#}");
        assert!(
            said.contains("spoolway init --workspace 'home-mode-stale-ws'"),
            "{said}"
        );
        assert!(said.contains(&missing.display().to_string()), "{said}");
    }

    /// The same hint, withheld: a workspace's gone entry recorded no root
    /// commit at all (an entry a version before that field existed wrote,
    /// or one for a `root` that never had any commits), so nothing proves
    /// the checkout asking is the clone that moved — `join_workspace` would
    /// never take this over either, so the hint must not promise it would.
    #[test]
    fn no_project_found_omits_a_stale_clone_with_no_root_commit_to_match() {
        let missing = crate::scratch::root("home-mode-stale-no-commit-gone");
        let (home, _home_guard) = workspace_fixture("home-mode-stale-no-commit", &missing, "api");
        let elsewhere = crate::scratch::root("home-mode-stale-no-commit-elsewhere");
        let _ = std::fs::remove_dir_all(&elsewhere);
        std::fs::create_dir_all(&elsewhere).unwrap();
        crate::scratch::git_init(&elsewhere, &["-b", "main"]);
        run(
            &elsewhere,
            "git",
            &["commit", "--allow-empty", "-q", "-m", "root"],
        )
        .unwrap();

        let err = crate::platform::test_home::with_home(&home, || Repo::discover(&elsewhere))
            .expect_err("an unlisted checkout is still not a project");
        let said = format!("{err:#}");
        assert!(
            !said.contains("--workspace"),
            "no entry here can be taken over, so no line should offer to: {said}"
        );
    }

    /// Six clones joining one workspace at once must all end up listed:
    /// [`join_workspace`] reads `project.toml`, adds its own entry, and
    /// writes the whole file back, with nothing serializing that
    /// read-modify-write against a sibling doing the same thing at the same
    /// time. Two joins racing between the read and the write each write
    /// back a `clones` list that is missing whichever entries landed after
    /// their own read — a lost update, not a crash, so every racer still
    /// reports success.
    #[test]
    fn six_concurrent_joins_all_end_up_listed() {
        let (home, _home_guard) = scratch_home("concurrent-joins");
        let first = crate::scratch::root("concurrent-joins-root-0");
        let _ = std::fs::remove_dir_all(&first);
        std::fs::create_dir_all(&first).unwrap();
        crate::scratch::git_init(&first, &["-q", "-b", "main"]);

        let workspace_name = crate::platform::test_home::with_home(&home, || {
            let clone = create_workspace(&first).expect("the first clone creates the workspace");
            clone
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        });

        let joiners = 5;
        let roots: Vec<crate::scratch::ScratchRoot> = (0..joiners)
            .map(|n| {
                let root = crate::scratch::root(&format!("concurrent-joins-root-{}", n + 1));
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(&root).unwrap();
                crate::scratch::git_init(&root, &["-q", "-b", "main"]);
                root
            })
            .collect();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(joiners));
        let handles: Vec<_> = roots
            .iter()
            .map(|root| root.to_path_buf())
            .map(|root| {
                let home = home.clone();
                let name = workspace_name.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    crate::platform::test_home::with_home(&home, || join_workspace(&root, &name))
                })
            })
            .collect();
        let results: Vec<Result<WorkspaceClone>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        for result in &results {
            assert!(
                result.is_ok(),
                "no joiner sees an error: {:?}",
                result.as_ref().err().map(|e| format!("{e:#}"))
            );
        }

        let workspace = home.join(".spoolway").join(&workspace_name);
        let parsed: WorkspaceToml =
            toml::from_str(&std::fs::read_to_string(workspace.join(BINDING_FILE)).unwrap())
                .unwrap();
        let listed: std::collections::BTreeSet<PathBuf> =
            parsed.clones.iter().map(|c| c.root.clone()).collect();
        let expected: std::collections::BTreeSet<PathBuf> = std::iter::once(first.to_path_buf())
            .chain(roots.iter().map(|r| r.to_path_buf()))
            .collect();
        assert_eq!(
            listed, expected,
            "every racing join must still be listed, not lost to a concurrent write"
        );
    }

    /// `WorkspaceLock::acquire` no longer `mkdir -p`s its lock file's
    /// parent, and both `join_workspace` and `move_clone` check a workspace
    /// exists before ever reaching it — so naming a workspace that was
    /// never created leaves nothing under `~/.spoolway/`, not even the
    /// folder the lock file would have sat in.
    #[test]
    fn joining_a_workspace_that_does_not_exist_creates_no_folder() {
        let (home, _home_guard) = scratch_home("join-missing-workspace");
        let root = crate::scratch::root("join-missing-workspace-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q", "-b", "main"]);

        crate::platform::test_home::with_home(&home, || {
            let err = join_workspace(&root, "never-created").err().unwrap();
            assert!(
                format!("{err:#}").contains("no workspace named never-created"),
                "{err:#}"
            );
        });
        assert!(
            !home.join(".spoolway").join("never-created").exists(),
            "a missing workspace must not be minted into existence by trying to join it"
        );
    }

    /// Acceptance: a join into a workspace whose `config/` has gone missing
    /// is refused, naming that path, rather than handed a dispatcher folder
    /// that reads nothing.
    #[test]
    fn joining_a_workspace_with_no_config_is_refused() {
        let (home, _home_guard) = scratch_home("join-no-config");
        let workspace = home.join(".spoolway").join("half-made");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join(BINDING_FILE),
            "id = \"half-made\"\nclones = []\n",
        )
        .unwrap();
        let root = crate::scratch::root("join-no-config-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q", "-b", "main"]);

        crate::platform::test_home::with_home(&home, || {
            let err = join_workspace(&root, "half-made").err().unwrap();
            assert!(
                format!("{err:#}").contains(&workspace.join("config").display().to_string()),
                "the refusal names the missing config/ path: {err:#}"
            );
        });
        assert!(
            !workspace.join("dispatchers").exists(),
            "no dispatcher folder is created for a workspace with no config/"
        );
    }

    /// The task's read-only case: a join into a workspace folder it cannot
    /// write fails naming a path inside that workspace, and leaves no
    /// `dispatchers/<name>` behind for a retry to step around as `<name>-2`.
    #[cfg(unix)]
    #[test]
    fn joining_a_read_only_workspace_names_the_path_and_leaves_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::scratch::root("join-read-only-root");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q", "-b", "main"]);
        let (home, _home_guard) =
            workspace_fixture("join-read-only", Path::new("/elsewhere"), "other");
        let workspace = home.join(".spoolway").join("join-read-only-ws");
        std::fs::create_dir_all(workspace.join("dispatchers")).unwrap();
        let before = std::fs::read_to_string(workspace.join(BINDING_FILE)).unwrap();

        std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores the mode bits, so there is nothing to test there.
        let writable = std::fs::write(workspace.join("probe"), "").is_ok();
        let result = (!writable).then(|| {
            crate::platform::test_home::with_home(&home, || {
                join_workspace(&root, "join-read-only-ws")
            })
        });
        std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o755)).unwrap();
        let Some(result) = result else { return };

        let err = format!(
            "{:#}",
            result.expect_err("a read-only workspace refuses the join")
        );
        assert!(
            err.contains(&workspace.display().to_string()),
            "the error names a path in the workspace: {err}"
        );
        assert_eq!(
            std::fs::read_dir(workspace.join("dispatchers"))
                .unwrap()
                .count(),
            0,
            "no dispatcher folder is left behind"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(BINDING_FILE)).unwrap(),
            before,
            "project.toml is untouched"
        );
    }

    /// One throwaway git checkout under its own scratch root, for the
    /// `move_clone` tests below — each needs several, none sharing a
    /// basename with another unless a test asks for exactly that.
    fn move_test_checkout(
        root_name: &str,
        basename: &str,
    ) -> (PathBuf, crate::scratch::ScratchRoot) {
        let base = crate::scratch::root(root_name);
        let root = base.join(basename);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q", "-b", "main"]);
        (root, base)
    }

    /// `move_clone`'s whole point: a clone moves to another workspace, its
    /// dispatcher folder — and whatever is in it — moving with it, and the
    /// workspace it came from no longer listing it.
    #[test]
    fn move_clone_moves_the_clone_and_its_dispatcher_folder() {
        let (home, _home_guard) = scratch_home("move-basic");
        let (root_a, _root_a_guard) = move_test_checkout("move-basic-a", "api");
        let (root_b, _root_b_guard) = move_test_checkout("move-basic-b", "other");

        let (from_workspace, from_home, to_name) =
            crate::platform::test_home::with_home(&home, || {
                let a = create_workspace(&root_a).unwrap();
                let b = create_workspace(&root_b).unwrap();
                let home_dir = a.home_dir();
                (
                    a.workspace,
                    home_dir,
                    b.workspace
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                )
            });
        std::fs::write(from_home.join("marker"), "queued-work").unwrap();

        let moved = crate::platform::test_home::with_home(&home, || {
            move_clone(&root_a, &to_name, None).unwrap()
        });

        assert_eq!(
            moved.workspace.file_name().unwrap().to_string_lossy(),
            to_name
        );
        assert!(
            moved.home_dir().join("marker").is_file(),
            "the dispatcher folder's own contents move with it"
        );
        assert!(!from_home.exists(), "the old dispatcher folder is gone");

        crate::platform::test_home::with_home(&home, || {
            let now = workspace_clone_checked(&root_a).unwrap().unwrap();
            assert_eq!(now.workspace, moved.workspace);
            let from_parsed: WorkspaceToml = toml::from_str(
                &std::fs::read_to_string(from_workspace.join(BINDING_FILE)).unwrap(),
            )
            .unwrap();
            assert!(
                from_parsed.clones.is_empty(),
                "the source workspace no longer lists the moved clone"
            );
        });
    }

    /// The whole point of the `git worktree repair` pass this runs after a
    /// successful move: a worktree cut under the clone's own dispatcher
    /// folder before the move rides along with it, and the main checkout's
    /// `.git/worktrees` resolves it at its new path afterwards, not the one
    /// the rename just carried the folder away from.
    #[test]
    fn move_clone_repairs_a_real_worktree_it_carried_across() {
        let (home, _home_guard) = scratch_home("move-repair");
        let (root_a, _root_a_guard) = move_test_checkout("move-repair-a", "api");
        std::fs::write(root_a.join("seed"), "seed\n").unwrap();
        run(&root_a, "git", &["add", "seed"]).unwrap();
        run(&root_a, "git", &["commit", "-qm", "seed"]).unwrap();
        let (root_b, _root_b_guard) = move_test_checkout("move-repair-b", "other");

        let (from_home, to_name) = crate::platform::test_home::with_home(&home, || {
            let a = create_workspace(&root_a).unwrap();
            let b = create_workspace(&root_b).unwrap();
            let from_home = a.home_dir();
            std::fs::create_dir_all(from_home.join("worktrees")).unwrap();
            run(
                &root_a,
                "git",
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "task/lane",
                    &from_home
                        .join("worktrees")
                        .join("lane")
                        .display()
                        .to_string(),
                ],
            )
            .unwrap();
            (
                from_home,
                b.workspace
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
        });
        assert!(
            slashed(run(&root_a, "git", &["worktree", "list"]).unwrap())
                .contains(&slashed(from_home.display().to_string())),
            "the fixture's own worktree is recorded under the old dispatcher folder to begin \
             with"
        );

        let moved = crate::platform::test_home::with_home(&home, || {
            move_clone(&root_a, &to_name, None).unwrap()
        });

        let new_worktree = moved.home_dir().join("worktrees").join("lane");
        assert!(
            new_worktree.is_dir(),
            "the worktree rode along with the move"
        );
        let listed = run(&root_a, "git", &["worktree", "list"]).unwrap();
        assert!(
            slashed(&listed).contains(&slashed(new_worktree.display().to_string())),
            "the main checkout resolves the worktree at its new path; got:\n{listed}"
        );
        assert!(
            !slashed(&listed).contains(&slashed(
                from_home
                    .join("worktrees")
                    .join("lane")
                    .display()
                    .to_string()
            )),
            "and no longer names the old one; got:\n{listed}"
        );
    }

    /// Acceptance criterion: a move is refused while a dispatcher is
    /// running over the clone's current folder, and leaves both workspaces
    /// untouched — it succeeds once that dispatcher stops.
    #[test]
    fn move_clone_refuses_while_a_dispatcher_is_running_then_succeeds_once_it_stops() {
        let (home, _home_guard) = scratch_home("move-locked");
        let (root_a, _root_a_guard) = move_test_checkout("move-locked-a", "api");
        let (root_b, _root_b_guard) = move_test_checkout("move-locked-b", "other");

        let (from_home, to_name) = crate::platform::test_home::with_home(&home, || {
            create_workspace(&root_a).unwrap();
            let b = create_workspace(&root_b).unwrap();
            let from_home = workspace_clone_checked(&root_a)
                .unwrap()
                .unwrap()
                .home_dir();
            (
                from_home,
                b.workspace
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
        });

        let lock = crate::lock::Lock::acquire(&from_home.join(crate::lock::LOCK_FILE), false, None)
            .unwrap();
        let err =
            crate::platform::test_home::with_home(&home, || move_clone(&root_a, &to_name, None))
                .unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot move while a dispatcher is running"),
            "{err:#}"
        );
        drop(lock);

        crate::platform::test_home::with_home(&home, || move_clone(&root_a, &to_name, None))
            .expect("the move succeeds once the dispatcher is gone");
    }

    /// A destination whose dispatcher folder already has the name the
    /// moved clone's folder carries is refused rather than silently
    /// renamed — `--dispatcher <name>` is what hands it over under a name
    /// chosen on purpose.
    #[test]
    fn move_clone_refuses_a_taken_dispatcher_name_until_one_is_chosen() {
        let (home, _home_guard) = scratch_home("move-collide");
        let (root_a, _root_a_guard) = move_test_checkout("move-collide-a", "api");
        let (root_b, _root_b_guard) = move_test_checkout("move-collide-b", "other");
        let (colliding, _colliding_guard) = move_test_checkout("move-collide-c", "api");

        let to_name = crate::platform::test_home::with_home(&home, || {
            create_workspace(&root_a).unwrap();
            let b = create_workspace(&root_b).unwrap();
            let name = b
                .workspace
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            join_workspace(&colliding, &name).unwrap();
            name
        });

        let err =
            crate::platform::test_home::with_home(&home, || move_clone(&root_a, &to_name, None))
                .unwrap_err();
        let said = format!("{err:#}");
        assert!(said.contains("--dispatcher"), "{said}");

        let moved = crate::platform::test_home::with_home(&home, || {
            move_clone(&root_a, &to_name, Some("api-2")).unwrap()
        });
        assert_eq!(moved.dispatcher, "api-2");
    }

    /// Acceptance criterion: a 0.6.0 repo-mode home — an ordinary `Binding`
    /// with no `clones` key — is never accepted by `move_clone` as a
    /// destination, the same as it is never listed in `init`'s menu.
    #[test]
    fn move_clone_refuses_a_0_6_0_repo_mode_home_as_a_destination() {
        let (home, _home_guard) = scratch_home("move-0-6-0-dest");
        let (root_a, _root_a_guard) = move_test_checkout("move-0-6-0-dest-a", "api");
        let repo_mode_home = home.join(".spoolway").join("repo-mode-home-abc123");
        std::fs::create_dir_all(&repo_mode_home).unwrap();
        std::fs::write(
            repo_mode_home.join(BINDING_FILE),
            "id = \"abc123\"\nroot = \"/some/other/checkout\"\n",
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            create_workspace(&root_a).unwrap();
            let err = move_clone(&root_a, "repo-mode-home-abc123", None).unwrap_err();
            let said = format!("{err:#}");
            assert!(
                said.contains("is not a workspace spoolway can move a clone into"),
                "{said}"
            );
            assert!(said.contains("spoolway init --setup home"), "{said}");
        });
    }
}
