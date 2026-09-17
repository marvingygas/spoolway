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
        let checkout = checkout_of(&start, &root, main.as_deref());
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
        let checkout = checkout_of(&start, &root, main.as_deref());
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
        if let Some(main) = main.filter(|dir| dir.join(crate::config::STATE_DIR).is_dir()) {
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
            .filter(|dir| *dir != state_root && dir.join(crate::config::STATE_DIR) != state_root)
            .find(|dir| dir.join(crate::config::STATE_DIR).is_dir());
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
        bail!(
            "no spoolway project found at or above {} (run `spoolway init` there first)",
            start.display()
        )
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
    /// spoolway writes: the queue, the archive, pending documents, scratch
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

    /// Where a producer leaves task documents for the queue screen to list:
    /// one flat directory of `.md` files, no subdirectories and no index.
    ///
    /// Created silently like every other sibling, so a project that has
    /// planned nothing still has a real, empty directory to read rather than
    /// a missing-path error on the first draw. Documents live here only until
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
    /// from. The one place that split is written down as code — see
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
            self.archive_dir(),
        ]
    }

    /// The tracked control plane's own directories, read from `checkout` —
    /// the branch actually running, not necessarily `root`'s.
    pub fn prompts_dir(&self) -> PathBuf {
        self.checkout.join(crate::config::PROMPTS_DIR)
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

    /// Where a project keeps task documents it wants to re-run — see
    /// [`crate::config::ROUTINES_DIR`] for why nothing auto-creates this the
    /// way [`Repo::pending_dir`] creates itself: an ordinary project that has
    /// saved no routine has no directory here at all, and the queue screen's
    /// `r` pane says so by naming this path rather than opening an empty one.
    pub fn routines_dir(&self) -> PathBuf {
        self.checkout.join(crate::config::ROUTINES_DIR)
    }

    pub fn task_templates_dir(&self) -> PathBuf {
        self.checkout.join(crate::config::TASK_TEMPLATES_DIR)
    }

    /// The project-scoped cron-job store, tracked in the checkout — read from
    /// `checkout` like every other tracked file above, so a lane sees its own
    /// branch's jobs. See [`crate::jobs`].
    pub fn jobs_file(&self) -> PathBuf {
        self.checkout.join(crate::config::JOBS_FILE)
    }

    /// Where a project overrides `epic.md` and `ticket.md`, the two bodies
    /// the `open` hook renders — see [`crate::task_template::resolve_tracking`].
    pub fn tracking_templates_dir(&self) -> PathBuf {
        self.checkout.join(crate::config::TRACKING_TEMPLATES_DIR)
    }

    pub fn pull_request_template_path(&self) -> PathBuf {
        self.checkout.join(crate::config::PULL_REQUEST_TEMPLATE)
    }

    /// Where a project overrides the seven typed messages a lane's pane
    /// receives — see [`crate::lane_prompts`].
    pub fn lane_prompts_path(&self) -> PathBuf {
        self.checkout.join(crate::config::LANE_PROMPTS_TEMPLATE)
    }

    /// Every task's lane state, across every dispatcher this machine has run
    /// for this project.
    pub fn lanes_file(&self) -> PathBuf {
        self.home().join(crate::dispatch::LANES_FILE)
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

    /// How many starts in a row could not run at all, and since when — see
    /// [`crate::lock::Restarts`].
    pub fn restarts_file(&self) -> PathBuf {
        self.home().join(crate::lock::RESTART_FILE)
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
    /// would not parse — for the dispatcher pass and the board, which name
    /// the bad file where a person will see it rather than leaving it only
    /// in the log.
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
/// the short form the `checkout:` line prints. Left absolute when `path` does
/// not sit under home, or when home cannot be resolved at all.
fn shorten_home(path: &Path) -> String {
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
/// `.git`, so its parent is that checkout. In the main checkout the two git
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
    // collapses to `None` exactly as it always did.
    common_git_dir(dir)
        .ok()
        .flatten()?
        .parent()
        .map(Path::to_path_buf)
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
    // failure to resolve one.
    let is_git_dir =
        common.file_name().is_some_and(|name| name == "git") || common.ends_with(".git");
    if !is_git_dir {
        return Ok(None);
    }
    // Canonicalized so it compares against the `start` every caller has
    // already canonicalized. On Windows the two spellings otherwise never
    // match: git prints `C:/Users/…` while `canonicalize` answers verbatim
    // `\\?\C:\…`, whose prefix component is a different thing — and with
    // that mismatch every main checkout read as a linked worktree.
    let canonical = common
        .canonical()
        .with_context(|| format!("resolving the git directory for {}", dir.display()))?;
    Ok(Some(canonical))
}

/// The file names a project's identity is stamped under, inside its common
/// git directory — see [`stamped_id`] and [`project_identity`].
const ID_FILE: &str = "spoolway-id";
const LABEL_FILE: &str = "spoolway-label";

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
    let checkout = common
        .parent()
        .with_context(|| format!("{} has no parent directory", common.display()))?
        .to_path_buf();
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
    let checkout = common
        .parent()
        .with_context(|| format!("{} has no parent directory", common.display()))?
        .to_path_buf();
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
/// by [`bind`]; nothing else ever writes this file except `spoolway init
/// --adopt`/`--new-id`, by a person's own request.
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
/// (acceptance criteria 2 and 7) is written outside a person's own
/// `--adopt`/`--new-id`.
fn write_binding(home: &Path, binding: &Binding) -> Result<()> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let body = format!(
        "# Which checkout this directory holds the state of, and the id its\n\
         # `.git` is stamped with. Checked against each other on every\n\
         # command — see the `binding-record` task. Updated on its own only\n\
         # to record a checkout that moved (the one it named is gone, or no\n\
         # longer carries this id); replaced outright only by a person\n\
         # running `spoolway init --adopt`/`--new-id` by hand.\n{}",
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
/// states the `binding-record` task defines, and the one place a home's
/// `project.toml` is written short of a person asking for it by name with
/// `spoolway init --adopt`/`--new-id`. Called from [`Repo::discover`] on
/// every command, not only `init`.
///
/// `root` is already canonicalized, as every caller's is.
pub(crate) fn bind(root: &Path) -> Result<PathBuf> {
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
            let record_line = match home_recording(root) {
                Some(home) => home.join(BINDING_FILE).display().to_string(),
                None => {
                    let fallback = crate::mux::state_root()
                        .join(crate::mux::project_label(root))
                        .join(BINDING_FILE);
                    format!(
                        "{} (does not exist — nothing records this checkout)",
                        fallback.display()
                    )
                }
            };
            bail!(
                "{} does not hold a usable id: {raw:?} is not six lowercase letters and \
                 digits\n  {}\n  fix it by hand, or run `spoolway init --new-id` in {} to \
                 mint a fresh one",
                stamp_path.display(),
                record_line,
                root.display(),
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
/// criteria 1 through 4 of `binding-record`, plus `migrate-legacy-home`'s
/// own criteria 2 (the retry after a blocked migration) and 4 (a legacy
/// home still sitting beside one already settled).
fn bind_stamped(root: &Path, id: &str) -> Result<PathBuf> {
    let home = crate::mux::project_home(root)?;
    let record_path = home.join(BINDING_FILE);
    // Already known to exist and parse: `read_stamp` just read it.
    let stamp_path = id_file_path(root)?.expect("a valid stamp was just read");

    let binding = match read_binding(&home) {
        Ok(Some(binding)) => binding,
        Ok(None) => {
            // `migrate_legacy_home` only ever mints a stamp after both of
            // its own liveness checks have already passed, so a checkout
            // reaching here already stamped, with no home to show for it,
            // was never refused by them — an earlier attempt at this same
            // migration minted the stamp and then could not finish moving
            // the directory: a crash, a kill, or the rename itself hitting
            // an unexpected filesystem error. Nothing here ever deletes the
            // legacy home, so it is still sitting at its own basename-keyed
            // path in that case, waiting for a retry (`migrate-legacy-home`
            // acceptance criterion 2) rather than the permanent "no home
            // holds the id" refusal below, which would otherwise wedge that
            // retry forever on a checkout this same process just stamped.
            let legacy = legacy_home_for(root);
            if legacy.is_dir() && read_legacy_pointer(&legacy).as_deref() == Some(root) {
                return migrate_legacy_home(root, &legacy);
            }

            // Criterion 4: a valid stamp, but no home records it at all —
            // either the directory itself is gone, or it exists but nobody
            // has ever bound a checkout to it. `--adopt {id}` is not
            // offered here: nothing under `~/.spoolway/` carries this id by
            // definition, so naming it back would send a person straight
            // into the same refusal a second time.
            bail!(
                "no home holds the id {id}\n  {}  {id}\n  nothing under {} records it\n  \
                 if a home under {} already holds this project's state under a different \
                 name, run `spoolway init --adopt <name>` naming it\n  \
                 `spoolway init --new-id` mints this checkout a fresh id and a fresh home \
                 instead",
                stamp_path.display(),
                record_path.display(),
                crate::mux::state_root().display(),
            );
        }
        Err(err) => {
            // The record is there but does not parse as a whole `Binding`
            // — most often a migration that renamed a legacy home straight
            // onto this exact `home` and was killed before it could
            // upgrade the record riding along with it, still carrying the
            // legacy shape: a `root`, no `id`. A stamp already valid here
            // is reason enough to finish that upgrade on the spot rather
            // than send a person chasing a parse error over an interrupted
            // move that is otherwise already done. What the record turns
            // out to be is [`reread_record`]'s call, not this error's:
            // between the read that failed and now, a racing migration may
            // have finished the very upgrade this arm exists to do.
            match reread_record(&home, root) {
                Reread::LegacyUpgrade => {
                    write_binding(
                        &home,
                        &Binding {
                            id: id.to_string(),
                            root: root.to_path_buf(),
                        },
                    )?;
                    return Ok(home);
                }
                // Somebody else's migration landed while this one was
                // reading: the error in hand describes a record that no
                // longer exists, so it is dropped and the whole `Binding`
                // standing there now goes down the ordinary agreement path
                // below, exactly as if the first read had seen it.
                Reread::Superseded(binding) => binding,
                Reread::Corrupt => return Err(err),
            }
        }
    };

    if binding.root == root && binding.id == id {
        // Criterion 1: both files already agree. Nothing to do — unless a
        // legacy home also independently claims this same checkout
        // (`migrate-legacy-home` acceptance criterion 4): choosing which of
        // two homes' queues is the real one is not this function's call to
        // make, so this refuses and names both rather than silently
        // preferring the one already settled.
        if let Some(legacy) = legacy_conflict(root, &home) {
            bail!(
                "two homes both claim {}: {} and {}\n  choose one by hand — nothing here \
                 merges them, or deletes either",
                root.display(),
                record_path.display(),
                legacy.join(BINDING_FILE).display(),
            );
        }
        return Ok(home);
    }

    if binding.root == root {
        // The record names this exact checkout, but a different id than
        // the stamp does — the stamp was edited by hand after the record
        // was written, or vice versa. Neither file is more likely right
        // than the other, so this refuses rather than silently trusting
        // one over the other.
        bail!(
            "{} and {} disagree about this checkout's id: the stamp says {id}, the record \
             says {}\n  `spoolway init --new-id` mints a fresh id both files will agree on",
            stamp_path.display(),
            record_path.display(),
            binding.id,
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
             {}  this one, stamped at {}\n  re-stamp this one with `spoolway init --new-id`",
            binding.root.display(),
            record_path.display(),
            root.display(),
            stamp_path.display(),
        ),
        Some(Err(err)) => {
            return Err(err).with_context(|| {
                format!(
                    "could not tell whether {} still carries the id {id} recorded in {} — \
                     refusing rather than guessing which checkout this binding belongs to. \
                     Fix whatever stopped that checkout's own stamp from being read (often a \
                     permissions problem) and run this again, or run `spoolway init --new-id` \
                     in {} to stop depending on the answer at all.",
                    binding.root.display(),
                    record_path.display(),
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

/// `bind`'s branch for a checkout carrying no stamp at all — acceptance
/// criteria 6 and 7 of `binding-record`, plus `migrate-legacy-home`'s own
/// migration itself and its criterion 4 (a legacy home still sitting beside
/// one already settled).
fn bind_unstamped(root: &Path) -> Result<PathBuf> {
    if let Some(home) = home_recording(root) {
        if let Some(legacy) = legacy_conflict(root, &home) {
            bail!(
                "two homes both claim {}: {} and {}\n  choose one by hand — nothing here \
                 merges them, or deletes either",
                root.display(),
                home.join(BINDING_FILE).display(),
                legacy.join(BINDING_FILE).display(),
            );
        }
        // Criterion 6: some home already names this exact checkout, but the
        // checkout itself carries no id to confirm it with — the stamp was
        // deleted or never made it into this clone. Refused rather than
        // silently re-stamped: writing a fresh id here would leave that
        // home's record pointing at an id nothing on disk carries any more.
        // `home` is already known by name, so `--adopt` is offered naming
        // exactly it, not a placeholder — the one refusal that can.
        let home_name = home
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| home.display().to_string());
        bail!(
            "{} has no id, but {} already records this checkout\n  \
             restore the stamp from the id in that file, or run \
             `spoolway init --adopt {home_name}` to re-stamp this checkout with it",
            common_git_dir(root)?
                .map(|dir| dir.join(ID_FILE).display().to_string())
                .unwrap_or_else(|| root.display().to_string()),
            home.join(BINDING_FILE).display(),
        );
    }

    // A 0.2 home, keyed on this checkout's plain basename the way every
    // home was before this stamp existed — see `legacy_home_for`. Checked
    // before minting anything: a checkout that turns out to have one is
    // never stamped by the ordinary path below at all, only by
    // `migrate_legacy_home` itself, and only once that call has actually
    // finished moving it (`migrate-legacy-home`'s own task, carried by
    // `binding-record`'s acceptance criterion 7 falling through to here).
    let legacy = legacy_home_for(root);
    if legacy.is_dir() {
        match read_legacy_pointer(&legacy) {
            Some(pointer_root) if pointer_root == root => {
                return migrate_legacy_home(root, &legacy);
            }
            // A legacy home sits exactly where this checkout's own basename
            // would look for one, but records a different root: this exact
            // checkout was itself moved to a new parent directory without
            // its basename changing (`legacy_home_for` finds it again by
            // that unchanged basename, but its recorded root is now the
            // old path), or a wholly different checkout that once lived
            // here simply shares this basename with the one now asking.
            // Either way there is a real, on-disk claim on this name that
            // this checkout does not itself hold — refused, naming
            // `--adopt` for the first case and `--new-id` for the second,
            // rather than silently minting a second, unrelated home right
            // beside it.
            //
            // This is *not* the `migrate-legacy-home` non-goal's renamed-
            // checkout case — a checkout whose *basename* changed along
            // with its path leaves no legacy home at this location to find
            // at all, and nothing on disk links the two: see
            // `legacy_home_for`'s own doc for why that one is left to
            // `spoolway init --adopt`, run by a person who still remembers
            // the old name, rather than anything guessed here.
            Some(pointer_root) => {
                let legacy_name = legacy
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| legacy.display().to_string());
                bail!(
                    "{} already holds a 0.2 home for a different checkout, recorded at {}\n  \
                     if that home is actually this checkout's own, moved since, run \
                     `spoolway init --adopt {legacy_name}` to claim it by hand\n  \
                     otherwise `spoolway init --new-id` mints this checkout a fresh home of \
                     its own",
                    legacy.join(BINDING_FILE).display(),
                    pointer_root.display(),
                );
            }
            // Not a legacy pointer at all — an unrelated directory that
            // merely shares this checkout's basename, or one that failed to
            // parse as anything recognisable. Neither is this function's to
            // referee; criterion 7 below proceeds exactly as it would if
            // nothing were here.
            None => {}
        }
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

/// The old, pre-`binding-record` home a checkout carrying no stamp of its
/// own may still be sitting under: `~/.spoolway/<basename>/`, keyed on the
/// checkout's current basename the same way every home was before this
/// stamp existed — see [`crate::mux::project_home`]'s own doc on the
/// fallback it still falls back to.
///
/// Not necessarily where a *0.2* home actually sits if the checkout's own
/// basename has changed since: this is the `migrate-legacy-home` task's own
/// "already renamed" non-goal, and it is genuinely undetectable, not merely
/// unhandled. A 0.2 checkout carries no stamp, and its legacy home's own
/// `project.toml` records only the *old* absolute path — once the checkout
/// has moved to a new one under a new basename, nothing on disk names both
/// paths together for anything here to find; scanning every home under
/// `~/.spoolway/` for one whose recorded root no longer exists would answer
/// with every abandoned or already-migrated 0.2 project on the machine, not
/// this one, which is exactly the guess the task's non-goal says not to
/// make. `spoolway init --adopt <name>` is the answer, run by a person who
/// still remembers the old name themselves.
///
/// A checkout moved to a *different parent* without its own basename
/// changing is not this case: `legacy_home_for` still finds the same
/// directory by that unchanged basename, and [`bind_unstamped`]'s own
/// caller refuses on it directly, since its recorded root will disagree
/// with the checkout's new one — see the `Some(pointer_root)` arm there.
fn legacy_home_for(root: &Path) -> PathBuf {
    crate::mux::state_root().join(crate::mux::project_label(root))
}

/// The `root` a 0.2-shaped `project.toml` at `home` records, read only when
/// `home` is genuinely that shape: a `root` and nothing recognisable as
/// [`Binding`]'s required `id`. `None` for everything else this might be
/// asked about — no file there, one that will not parse at all, or one that
/// already carries a valid `id` and so is an ordinary [`Binding`], already
/// migrated — so a caller never mistakes an already-migrated home, or some
/// unrelated directory that merely happens to share a project's basename,
/// for one still waiting to move.
fn read_legacy_pointer(home: &Path) -> Option<PathBuf> {
    // An id already present is an ordinary `Binding`, not this — checked
    // first so an already-migrated home is never read as one still
    // waiting to be.
    if read_binding(home).ok().flatten().is_some() {
        return None;
    }
    #[derive(Deserialize)]
    struct LegacyPointer {
        root: PathBuf,
    }
    let raw = std::fs::read_to_string(home.join(BINDING_FILE)).ok()?;
    let pointer: LegacyPointer = toml::from_str(&raw).ok()?;
    Some(pointer.root)
}

/// What a `project.toml` that would not parse as a whole [`Binding`] turns
/// out to be, asked a second time — see [`reread_record`], whose three
/// answers these are.
#[derive(Debug)]
enum Reread {
    /// The legacy shape, naming this same checkout: a migration that moved
    /// the home and never got to upgrade the record riding along with it.
    LegacyUpgrade,
    /// A whole `Binding` after all. Not what the first read saw, so not a
    /// record that was ever corrupt — a racing migration finished its own
    /// upgrade in between.
    Superseded(Binding),
    /// Neither shape: genuinely corrupt, and whatever error the first read
    /// reported for it still stands.
    Corrupt,
}

/// A second look at a record that would not parse, with the checkout it was
/// read for in hand.
///
/// The read that failed is not enough to judge on its own. Every racer
/// resolving an upgraded 0.2 project's home for the first time reads this
/// same file, and the winner's [`write_binding`] lands between some
/// loser's own read and the recovery that follows it: that loser is
/// holding a parse error over a legacy record the winner has already
/// replaced with a whole `Binding`. Nothing is corrupt and nothing is left
/// to migrate — so this answers from what the file says *now* rather than
/// from the superseded error, and [`read_legacy_pointer`] declining a
/// record that already parses as a `Binding` is read as exactly that,
/// rather than as "not the legacy shape either, so corrupt".
fn reread_record(home: &Path, root: &Path) -> Reread {
    match read_legacy_pointer(home) {
        Some(pointer_root) if pointer_root == root => Reread::LegacyUpgrade,
        _ => match read_binding(home) {
            Ok(Some(binding)) => Reread::Superseded(binding),
            _ => Reread::Corrupt,
        },
    }
}

/// A legacy home genuinely conflicts with `home` — an id-keyed home already
/// settled as this checkout's own — only when it is real: not a candidate
/// still waiting for its first migration (that case never reaches this; it
/// is [`migrate_legacy_home`]'s to move, not refuse over), but one sitting
/// alongside a home already bound a different way, through `--adopt` or a
/// migration this checkout never got the chance to run because something
/// else recorded it first. `migrate-legacy-home` acceptance criterion 4:
/// refuse and name both rather than merge or pick.
fn legacy_conflict(root: &Path, home: &Path) -> Option<PathBuf> {
    let legacy = legacy_home_for(root);
    if legacy == *home {
        return None;
    }
    match read_legacy_pointer(&legacy) {
        Some(pointer_root) if pointer_root == root => Some(legacy),
        _ => None,
    }
}

/// Whether a worktree cut under `home` is genuinely in use right now — the
/// general signal [`migrate_legacy_home`] refuses on, covering every
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
/// the process — exactly what a legacy home's own move does to a worktree
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
/// instead of `/proc`: macOS and Windows have no file for this to read
/// directly (libproc and a PEB read, respectively), and `sysinfo` already
/// carries both behind one call, refreshed for cwd alone rather than every
/// metric it can report. A process caught mid-exit is not a concern here
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

/// Move `legacy` — a 0.2 home confirmed by [`read_legacy_pointer`] to
/// belong to `root` — onto the id this checkout is about to be (or already
/// was) stamped with, the one time it is ever needed. See the
/// `migrate-legacy-home` task.
///
/// Called from [`bind_unstamped`], before this checkout is stamped at all,
/// and from [`bind_stamped`]'s own "no home holds the id" branch, for a
/// checkout an earlier attempt already stamped but could not finish moving.
/// Refusing here must never mint a stamp: a checkout left unstamped is what
/// lets the very next command land back in [`bind_unstamped`] and try the
/// whole thing again, rather than wedging forever on `bind_stamped`'s own
/// "no home holds the id" bail once something else has already written the
/// stamp this call would otherwise write itself.
///
/// Refuses, leaving `legacy` untouched, while [`crate::lock::Lock::holder`]
/// reports a live dispatcher over it, or while [`any_worktree_in_use`]
/// finds a live process still working in one of its worktrees — a headless
/// lane survives its own dispatcher's death by design, so the two checks
/// are independent, not one covering the other. Moving the directory out
/// from under either pulls the ground out from under real, live work
/// (acceptance criterion 2). Every *other* worktree under the legacy home
/// rides along with the move regardless of whether it is live — nothing
/// here can tell, and nothing needs to: `git worktree repair` afterwards is
/// what lets it still resolve from the main checkout (acceptance criterion
/// 3).
fn migrate_legacy_home(root: &Path, legacy: &Path) -> Result<PathBuf> {
    if let Some(pid) = crate::lock::Lock::holder(&legacy.join(crate::lock::LOCK_FILE))? {
        bail!(
            "{} cannot move while work is live\n  dispatcher running   pid {pid}\n  the old \
             home is untouched at {}\n  run this again once the dispatch finishes",
            legacy.display(),
            legacy.display(),
        );
    }
    if any_worktree_in_use(legacy) {
        bail!(
            "{} cannot move while work is live\n  a worktree under it is checked out\n  \
             the old home is untouched at {}\n  run this again once that work has finished",
            legacy.display(),
            legacy.display(),
        );
    }

    let Some((id, _minted)) = stamped_id(root)? else {
        bail!(
            "{} has no git repository behind it — spoolway keys a project's home off an id \
             stamped into its own `.git`, so there is nowhere to write one. Run `git init` \
             here first.",
            root.display()
        );
    };
    let home = crate::mux::project_home(root)?;

    // Every worktree cut under the legacy home, by its own current absolute
    // path — read before anything moves, since after the rename below
    // `legacy` no longer resolves to anything a `git worktree list` call
    // could look up. Only the plain default location is ever a *worktree*
    // of this home rather than an unrelated directory a person happened to
    // create beside `queue/`: a configured `dispatch.worktree_root` is an
    // absolute path of its own, outside `legacy` entirely, and never moves
    // with it — nothing under it needs repairing at all.
    let default_worktrees = !Config::load_tracked(root)
        .map(|config| !config.dispatch.worktree_root.trim().is_empty())
        .unwrap_or(false);
    let moved_worktrees: Vec<PathBuf> = if default_worktrees {
        std::fs::read_dir(legacy.join("worktrees"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    } else {
        Vec::new()
    };

    let moved = rename_onto_home(legacy, &home)?;

    // Written at `home`, never at `legacy` — the directory the rename above
    // either just moved or (on the losing side of the race just above)
    // already found moved. Idempotent regardless: every racer agrees on
    // the same `id`, so writing it again here only ever repeats content
    // already there. Writing to `legacy` instead, before the rename, was
    // tried first and found to race itself: a second call's own write
    // there could still be in flight — its own temp file not yet linked
    // into place — the instant a first call's rename carried the directory
    // holding it away, which fails that second call's write with a bare
    // `ENOENT` neither call caused on purpose. `home` is never renamed out
    // from under a write the way `legacy` is, because nothing renames it
    // anywhere once it is `home`.
    write_binding(
        &home,
        &Binding {
            id,
            root: root.to_path_buf(),
        },
    )?;

    if moved {
        if !moved_worktrees.is_empty() {
            // `repair`, unlike an ordinary `git` command, only fixes a
            // worktree whose new location it is actually told — run with no
            // arguments from the main checkout it repairs nothing a stale
            // path cannot already resolve on its own. Each moved worktree's
            // own new absolute path, under `home` rather than `legacy`, is
            // what tells it (acceptance criterion 3).
            let new_paths: Vec<String> = moved_worktrees
                .iter()
                .filter_map(|old| old.strip_prefix(legacy).ok())
                .map(|rel| home.join(rel).display().to_string())
                .collect();
            let mut args: Vec<&str> = vec!["worktree", "repair"];
            args.extend(new_paths.iter().map(String::as_str));
            // Best-effort in outcome, not in visibility: a move that
            // otherwise succeeded must not fail over housekeeping a person
            // can always rerun by hand, but a failure here is a real thing
            // to know about, not a reason to pretend the worktrees are
            // fine — silently discarding it left exactly that finding
            // unfixed once.
            if let Err(err) = run(root, "git", &args) {
                // Quoted with this platform's own shell syntax, not joined
                // bare: a normal home or checkout path can carry a space (a
                // person's own username, most often), and an unquoted
                // command a person cannot paste back verbatim fails the
                // person-facing error-message standard just as surely as a
                // missing path does.
                let quoted_paths: Vec<String> = new_paths
                    .iter()
                    .map(|path| crate::platform::Shell::CURRENT.quote(path))
                    .collect();
                println!(
                    "  worktree repair failed: {err:#}\n  run this by hand in {}:\n    git \
                     worktree repair {}",
                    root.display(),
                    quoted_paths.join(" "),
                );
            }
        }
        println!("  moved  {}  ->  {}/", legacy.display(), home.display());
        println!(
            "         {}",
            crate::commands::init::home_inventory_line(root, &home)
        );
    }
    Ok(home)
}

/// How long [`rename_onto_home`] keeps retrying a rename Windows is refusing
/// over an open handle, and how long it pauses between attempts. Long enough
/// for a loaded runner, and short enough that a rename genuinely stuck — a
/// destination already standing with content of its own, say — reports its
/// own error rather than hanging on it.
const RENAME_PATIENCE: std::time::Duration = std::time::Duration::from_millis(500);
const RENAME_PAUSE: std::time::Duration = std::time::Duration::from_millis(20);

/// Move `legacy` onto `home`, answering whether *this* call is the one that
/// moved it — `false` meaning another racer got there first and there is
/// nothing left to do but agree.
///
/// The lost race is read off the outcome, never off one errno: every racer
/// computes the identical destination (`stamped_id`'s own atomic hard-link
/// means they all agree on one `id`, see its own doc), so a legacy home
/// gone with `home` standing as a directory *is* that migration, whatever
/// this particular call's rename happened to report. Keying it on `ENOENT`
/// alone is what shipped wrong: a losing racer on Windows is told
/// `ERROR_ACCESS_DENIED`, not that the source is missing, and surfaced a
/// bare "Access is denied" out of a migration that had in fact just
/// succeeded beside it.
///
/// Windows contention is its own case, separate from a lost race, and the
/// reason anything waits here: a directory there cannot be renamed while
/// anything holds a handle inside it, and a racer still reading the legacy
/// home — or the winner's own rename, mid-flight — is exactly that. Failing
/// on the first `ERROR_ACCESS_DENIED` would refuse a call whose turn had
/// simply not come yet, so the rename is retried over a bounded window and
/// whichever way the contention settles is answered above: this call moves
/// it, or finds it moved. Unix has no such window — a rename there either
/// succeeds or has already lost — so nothing waits.
fn rename_onto_home(legacy: &Path, home: &Path) -> Result<bool> {
    let mut waited = std::time::Duration::ZERO;
    loop {
        let err = match std::fs::rename(legacy, home) {
            Ok(()) => return Ok(true),
            Err(err) => err,
        };
        if !legacy.exists() && home.is_dir() {
            return Ok(false);
        }
        if !contended(&err, cfg!(windows)) || waited >= RENAME_PATIENCE {
            return Err(err)
                .with_context(|| format!("moving {} to {}", legacy.display(), home.display()));
        }
        std::thread::sleep(RENAME_PAUSE);
        waited += RENAME_PAUSE;
    }
}

/// Whether `err` is Windows refusing a rename it may well allow a moment
/// later, rather than a failure worth reporting.
///
/// `windows` is passed in rather than read from `cfg!` in here so that a
/// test can ask for both answers on one platform: this crate's own rule
/// that a `#[cfg(windows)]` body is never built on Linux CI (see
/// [`crate::platform`]), applied to a judgement rather than to a path.
///
/// `ERROR_ACCESS_DENIED` is the one actually seen — `os error 5`, from
/// racing migrations on `windows-latest`. `ERROR_SHARING_VIOLATION` is the
/// same contention under Win32's other spelling for it, named by number
/// because Rust maps it to no `ErrorKind` of its own. Neither counts off
/// Windows, where `PermissionDenied` on a rename means what it says and
/// waiting on it would only delay the error.
fn contended(err: &std::io::Error, windows: bool) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    windows
        && (err.kind() == std::io::ErrorKind::PermissionDenied
            || err.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

/// Overwrite `root`'s own stamp with `id`, whatever it already held —
/// [`read_or_mint`]'s idempotent read is exactly what [`adopt`] and
/// [`restamp`] must not get, since both exist to force a disagreement
/// straight rather than read back whatever was already there.
///
/// `label`, unlike `id`, is `None` for [`restamp`]: a fresh id does not
/// mean a fresh name, so the checkout's own label is left alone once it
/// exists (frozen at a checkout's first stamp by design, see
/// [`stamped_id`]), and only written at all for one stamped for the very
/// first time, which needs one for [`crate::mux::project_home`] to key
/// off. [`adopt`] passes `Some`, forcing the label to match — the home
/// being adopted may carry a different one than this checkout's own
/// basename, and [`crate::mux::project_home`] has to key off *that* label
/// afterwards or a checkout adopting `api-8w4r2c` while its own current
/// basename is `fresh` would resolve straight back to `fresh-8w4r2c`, a
/// home nothing wrote, the moment anything asks again.
fn stamp_over(root: &Path, id: &str, label: Option<&str>) -> Result<()> {
    let common = common_git_dir(root)?.with_context(|| {
        format!(
            "{} has no git repository behind it — spoolway keys a project's home off an id \
             stamped into its own `.git`.",
            root.display()
        )
    })?;
    crate::task::write_atomic(&common.join(ID_FILE), id)?;
    match label {
        Some(label) => {
            crate::task::write_atomic(&common.join(LABEL_FILE), label)?;
        }
        None if peek(&common.join(LABEL_FILE), is_valid_label)?.is_none() => {
            crate::task::write_atomic(
                &common.join(LABEL_FILE),
                sanitize_label(&crate::mux::project_label(root)),
            )?;
        }
        None => {}
    }
    Ok(())
}

/// `spoolway init --adopt <name>`: bind `root` to the home already at
/// `~/.spoolway/<name>/`, overwriting the checkout's own stamp and that
/// home's own record to match — the one way two disagreeing files are made
/// to agree on a person's own say-so rather than [`bind`]'s own judgement,
/// which never does more than record a move or refuse (see the
/// `binding-record` task's non-goals). `name` is the home's own directory
/// name, `<label>-<id>` — the mockup's own `spoolway init --adopt
/// api-8w4r2c`, not the bare id alone: a bare id can be handed straight to
/// [`is_valid_id`] and joined without a lookup, but a home a person is
/// pointing at by name may have been renamed by hand, or may be a home
/// this checkout has never carried a matching id for at all — the very
/// case `--adopt` exists for.
///
/// A 0.2 home is the one exception to that `<label>-<id>` shape, and it
/// is taken too: named by the plain basename it was filed under before an
/// id keyed anything, it is carried onto this checkout's own id through
/// [`migrate_legacy_home`], with the same liveness refusals, the same
/// `git worktree repair` pass and the same never-delete guarantee the
/// automatic route has. This is the answer the `migrate-legacy-home`
/// non-goal names for a checkout renamed under 0.2, whose old home
/// nothing on disk still links to its new path — so it is deliberately
/// the one route that does *not* require the recorded root to match
/// `root`, since naming a path that no longer exists is exactly the case
/// it exists for.
///
/// `name` is validated as an ordinary, single path component before
/// anything is built from it: a separator, a `..`, or a character outside
/// what a directory name can hold must never reach a path joined onto
/// `state_root()`.
pub(crate) fn adopt(root: &Path, name: &str) -> Result<PathBuf> {
    if !crate::tracking::is_bare_filename(name) {
        bail!(
            "`{name}` is not a plain directory name, so it cannot name a home under {} — an \
             id can never carry a path separator or a `..`. Run `spoolway init --adopt <name>` \
             again with the home's own directory name, exactly as `ls {}` lists it.",
            crate::mux::state_root().display(),
            crate::mux::state_root().display(),
        );
    }
    let home = crate::mux::state_root().join(name);
    if !home.is_dir() {
        bail!(
            "no home named {name} exists under {} — check `ls {}` for the name actually there, \
             or run `spoolway init --new-id` to bind this checkout to a fresh home instead of \
             adopting an existing one.",
            crate::mux::state_root().display(),
            crate::mux::state_root().display(),
        );
    }
    // A 0.2 home, named the way every home was before an id keyed one:
    // the plain basename the checkout had back then, and a `project.toml`
    // carrying a `root` and no `id` at all. This is the one route a person
    // has to a legacy home `bind` itself can no longer find — the
    // `migrate-legacy-home` non-goal's renamed-under-0.2 checkout, whose
    // old home sits under a basename nothing on disk still links to the
    // new one — and it is the route every refusal in this area names, so
    // it has to actually work on the shape it is pointed at. Handled here,
    // before the `<label>-<id>` split below, because a legacy name answers
    // that split wrongly twice over: it has no `-<id>` suffix to find, and
    // one that merely contains a `-` (`my-project`) would split into a
    // label and an "id" that were never either.
    //
    // Delegated whole to `migrate_legacy_home` rather than reimplemented:
    // adopting a legacy home *is* the migration, just asked for by hand
    // instead of found automatically, and it must carry the same liveness
    // refusals, the same `git worktree repair` pass and the same
    // never-delete guarantee with it. The destination follows this
    // checkout's own current label, not `name` — a checkout renamed from
    // `api` to `billing` adopts `~/.spoolway/api/` onto
    // `~/.spoolway/billing-<id>/`, which is the whole point of adopting it.
    //
    // The recorded root is deliberately not required to match `root`: it
    // naming a path that no longer exists is exactly the case this exists
    // for, and `--adopt` is already the explicit, typed-by-a-person
    // override for a link spoolway cannot make on its own.
    if read_legacy_pointer(&home).is_some() {
        return migrate_legacy_home(root, &home);
    }

    // `name` is `<label>-<id>` by construction — every home this project
    // ever wrote is named that way — so splitting on the last `-` recovers
    // both halves regardless of which one ends up actually used below.
    let (name_label, name_id) = match name.rsplit_once('-') {
        Some((label, id)) => (label, Some(id)),
        None => (name, None),
    };
    // `name` passing `is_bare_filename` only proves the whole string is one
    // plain path component — splitting it on its last `-` can still strand
    // a label half that is not, such as the empty label `-abc123` splits
    // into. `stamp_over` writes `name_label` into `spoolway-label`
    // unchecked, and `project_home` joins it straight onto `state_root()`,
    // so an unusable label here would only surface the next time this
    // checkout is resolved — refuse it now, before anything is written.
    if !is_valid_label(name_label) {
        bail!(
            "{name} is not a usable home name — splitting it on its last `-` leaves the label \
             {name_label:?}, which is not a plain directory name, so re-running `spoolway init \
             --adopt {name}` cannot succeed. Rename {} to a `<label>-<id>` name with a real \
             label before adopting it, or run `spoolway init --new-id` in this checkout \
             instead to bind a fresh home rather than adopting this one.",
            home.display(),
        );
    }
    // The id this home is keyed on: read back from its own record when it
    // has one — the only place a home's id is written down apart from its
    // own directory name — and otherwise trust the name's own suffix, for
    // a home that has a directory but no `project.toml` of its own yet.
    // Either way, validated before it is stamped anywhere: a hand-edited
    // record is exactly what must not silently mint a checkout an
    // unusable or disagreeing id.
    let record_path = home.join(BINDING_FILE);
    let id = match read_binding(&home)? {
        Some(binding) => {
            if !is_valid_id(&binding.id) {
                bail!(
                    "{} carries an id that is not six lowercase letters and digits: {:?} — \
                     fix it by hand, or run `spoolway init --new-id` in {} to mint this \
                     checkout a fresh id and a fresh home instead of adopting this one.",
                    record_path.display(),
                    binding.id,
                    root.display(),
                );
            }
            if let Some(name_id) = name_id
                && name_id != binding.id
            {
                bail!(
                    "{} is named for the id {name_id}, but {} records the id {} — fix one to \
                     match the other by hand before adopting it, or run `spoolway init \
                     --new-id` in {} to sidestep both.",
                    home.display(),
                    record_path.display(),
                    binding.id,
                    root.display(),
                );
            }
            binding.id
        }
        None => {
            let Some(name_id) = name_id else {
                bail!(
                    "{} carries no {} of its own, and its name has no `-<id>` suffix either, \
                     so there is no id to stamp this checkout with — run `spoolway init \
                     --new-id` in {} instead to mint one from scratch.",
                    home.display(),
                    record_path.display(),
                    root.display(),
                );
            };
            if !is_valid_id(name_id) {
                bail!(
                    "{} carries no {} of its own, and its name's own id, {name_id:?}, is not \
                     six lowercase letters and digits — fix the name by hand, write a valid \
                     {} yourself, or run `spoolway init --new-id` in {} instead.",
                    home.display(),
                    record_path.display(),
                    record_path.display(),
                    root.display(),
                );
            }
            name_id.to_string()
        }
    };
    stamp_over(root, &id, Some(name_label))?;
    write_binding(
        &home,
        &Binding {
            id,
            root: root.to_path_buf(),
        },
    )?;
    Ok(home)
}

/// `spoolway init --new-id`: mint `root` a fresh id it has never carried
/// before, and bind it to the fresh home that id keys — the other of the
/// two ways a person forces a disagreement straight, for the checkout that
/// would rather stop sharing an id than fight over who it belongs to.
pub(crate) fn restamp(root: &Path) -> Result<PathBuf> {
    let id = generate_id();
    stamp_over(root, &id, None)?;
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
/// letters-and-digits. `pub(crate)` for [`adopt`], which parses one back
/// out of a home directory's own `<label>-<id>` suffix rather than trust
/// it blind — the id-shaped alphabet this checks against is itself what
/// keeps a parsed suffix from ever being able to escape `~/.spoolway/`
/// (the acceptance criterion this alphabet exists to satisfy); `adopt`'s
/// own escape guard on the *name* it is actually handed is
/// [`crate::tracking::is_bare_filename`], a separate, wider check, since a
/// home's directory name is not required to end in a valid id at all.
pub(crate) fn is_valid_id(candidate: &str) -> bool {
    candidate.len() == ID_LEN
        && candidate
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A label safe to `join` onto `state_root()` unchanged: one normal path
/// component, never a separator, `.`/`..`, or a Windows drive-relative
/// spelling like `C:evil` — the same rule `crate::tracking::is_bare_filename`
/// enforces on a hook name for the same reason. A corrupted `spoolway-label`
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

fn git_toplevel(dir: &Path) -> Result<PathBuf> {
    let out = run(dir, "git", &["rev-parse", "--show-toplevel"])?;
    // The same spelling rule as `main_checkout`: git's answer, in the form
    // `canonicalize` would give, so paths derived from either compare equal.
    let top = PathBuf::from(out.trim());
    top.canonical()
        .with_context(|| format!("resolving {}", top.display()))
}

/// Run a command in `cwd` and return its stdout, or an error carrying stderr.
pub fn run(cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn scratch_home(name: &str) -> PathBuf {
        let home = crate::scratch::root(&format!("repo-test-home-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home.canonical().unwrap()
    }

    /// `Repo::discover`, under a scratch home of its own — never the real
    /// `~/.spoolway/`. Binding is automatic now (`Repo::discover` calls
    /// `bind` itself, criterion 7: nothing recorded, no stamp, binds on its
    /// own), so there is nothing left to claim first.
    fn discover_registered(work: &Path) -> Result<Repo> {
        discover_registered_as(work, work)
    }

    /// The same, started from `start` — a linked worktree of `_project`, in
    /// the tests that need one. `_project` is unused now that binding is
    /// automatic; kept as a parameter so every call site naming the project
    /// a worktree belongs to still reads that way.
    fn discover_registered_as(_project: &Path, start: &Path) -> Result<Repo> {
        let home = scratch_home("registered");
        crate::platform::test_home::with_home(&home, || Repo::discover(start))
    }

    /// A bare "origin", a checkout wired to it, and spoolway state in the
    /// checkout. Real git throughout: these are the behaviours that only break
    /// against the real thing.
    fn fixture(name: &str) -> (PathBuf, PathBuf) {
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

        (origin, work)
    }

    /// The retention sweep is allowed to delete anything on this list, so
    /// the checkout's tracked prompt templates must never appear on it. They
    /// did once, when `prompts/` still named the composed-prompt directory,
    /// and the sweep deleted checked-in files.
    #[test]
    fn the_tracked_prompt_templates_are_not_a_byproduct_directory() {
        let (_origin, work) = fixture("byproducts");
        let repo = discover_registered(&work).unwrap();
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
        let (_origin, work) = fixture("worktree");
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

        let repo = discover_registered_as(&work, &wt).unwrap();
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
        let (_origin, work) = fixture("git-dir");
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

    /// The one fact every `checkout:` line and its `--json` twin read off of:
    /// a worktree's own absolute path and the branch it has out — not the
    /// project's.
    #[test]
    fn checkout_note_names_the_worktree_and_its_branch() {
        let (_origin, work) = fixture("checkout-note");
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

        let repo = discover_registered_as(&work, &wt).unwrap();
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
        let (_origin, work) = fixture("checkout-note-main");
        let repo = discover_registered(&work).unwrap();
        assert!(repo.checkout_note().unwrap().is_none());
    }

    #[test]
    fn discover_from_the_project_itself_is_unchanged() {
        let (_origin, work) = fixture("plain");
        let repo = discover_registered(&work).unwrap();
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
        let (_origin, work) = fixture("unparsable");
        std::fs::write(
            work.join(crate::config::STATE_DIR).join("config.toml"),
            "this is not = = toml\n",
        )
        .unwrap();

        let home = scratch_home("unparsable");
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
    #[cfg(unix)]
    #[test]
    fn a_home_resolution_failure_does_not_stop_lenient_discovery() {
        use std::os::unix::fs::PermissionsExt;
        let (_origin, work) = fixture("home-resolution-failure");
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
        let (_origin, work) = fixture("worktree-lookup");
        let repo = discover_registered(&work).unwrap();

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
        let (_origin, work) = fixture("dependency-branch-fallback");
        git(&work, &["branch", "task/aged"]);
        let repo = discover_registered(&work).unwrap();
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

        let repo = discover_registered(&base).unwrap();
        assert!(
            !repo.has_remote(),
            "local-only repos must not attempt a push"
        );
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

        let repo = discover_registered(&sub).unwrap();
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
            checkout: base.clone(),
            root: base.clone(),
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
        let home = scratch_home("global-state-root");
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

        let home = scratch_home("walk-bound");
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
        let (_origin, work) = fixture("unbound");
        let home = scratch_home("unbound");

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
    fn bind_fixture(name: &str) -> PathBuf {
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
        let home = scratch_home("criterion-1");
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
        let home = scratch_home("criterion-1b");
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
        assert!(said.contains("--new-id"), "{said}");
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
        let home = scratch_home("criterion-2");

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
    /// since changed to something else — re-stamped by hand, or by
    /// `--new-id` — moves to this one exactly as a gone checkout does. Not
    /// reachable through `rename` the way the first cause is, so this
    /// writes the disagreeing files directly.
    #[test]
    fn bind_criterion_2_the_other_cause_a_checkout_that_no_longer_carries_the_id_moves_to_this_one()
    {
        let base = crate::scratch::root("bind-criterion-2b");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let home = scratch_home("criterion-2b");

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
            stamp_over(&old_checkout, "aaaaaa", None).unwrap();
            stamp_over(&new_checkout, "bbbbbb", None).unwrap();
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
        let home = scratch_home("criterion-3");

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
        assert!(said.contains("--new-id"), "{said}");
    }

    /// A real failure reading the recorded checkout's own stamp — a
    /// permissions problem here, a corrupt repository in general — must
    /// refuse rather than being read the same as "no longer carries the
    /// id" and silently taken as licence to transfer the binding: neither
    /// proceeding nor guessing is allowed, only recording a move whose
    /// cause is actually known (see the task's non-goals).
    #[cfg(unix)]
    #[test]
    fn bind_an_indeterminate_read_of_the_other_checkout_refuses_rather_than_guessing() {
        use std::os::unix::fs::PermissionsExt;

        let base = crate::scratch::root("bind-indeterminate");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let original = base.join("original");
        std::fs::create_dir_all(&original).unwrap();
        crate::scratch::git_init(&original, &["-b", "plan/demo"]);
        let home = scratch_home("indeterminate");

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
    /// refuse, naming both files and the two commands that resolve it.
    #[test]
    fn bind_criterion_4_a_valid_stamp_with_no_home_refuses() {
        let work = bind_fixture("criterion-4");
        let home = scratch_home("criterion-4");
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
        assert!(
            said.contains("--adopt") && said.contains("--new-id"),
            "{said}"
        );
    }

    /// Criterion 5: a stamp that is not six lowercase base36 characters
    /// refuses, naming the format rather than treating it as unstamped.
    #[test]
    fn bind_criterion_5_a_malformed_stamp_refuses_naming_the_format() {
        let work = bind_fixture("criterion-5");
        let home = scratch_home("criterion-5");
        let git_dir = work.join(".git");
        std::fs::write(git_dir.join("spoolway-id"), "NOT-VALID!!\n").unwrap();

        let err = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect_err("a malformed stamp must be refused, not read as unstamped");
        let said = format!("{err:#}");
        assert!(
            said.contains("not six lowercase letters and digits"),
            "{said}"
        );
        assert!(said.contains("--new-id"), "{said}");
    }

    /// Criterion 6: no stamp, but some home already records this exact
    /// checkout — the stamp was deleted or never made it into this clone —
    /// refuses rather than silently minting a fresh id that home's record
    /// would then disagree with.
    #[test]
    fn bind_criterion_6_no_stamp_where_a_home_already_records_this_path_refuses() {
        let work = bind_fixture("criterion-6");
        let home = scratch_home("criterion-6");
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
        assert!(said.contains("--adopt"), "{said}");
    }

    /// Criterion 7: no stamp, and nothing records this checkout anywhere —
    /// there is nothing to guess, so it binds itself and proceeds. Already
    /// exercised through `Repo::discover` by
    /// `discovery_binds_a_project_nobody_has_bound_before`; this is the
    /// same fact at `bind`'s own level.
    #[test]
    fn bind_criterion_7_nothing_recorded_anywhere_binds_itself() {
        let work = bind_fixture("criterion-7");
        let home = scratch_home("criterion-7");
        let bound = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("nothing recorded anywhere binds itself and proceeds");
        assert!(bound.join(BINDING_FILE).is_file());
        assert!(work.join(".git").join("spoolway-id").is_file());
    }

    /// A 0.2 home for `work`: `~/.spoolway/<basename>/project.toml`
    /// carrying only a `root`, the shape every home had before
    /// `binding-record`'s id-keyed one existed. Must run inside
    /// [`crate::platform::test_home::with_home`], the same as `bind` itself
    /// — the legacy home this writes is found by the real `$HOME` `bind`
    /// resolves against, not by any path handed back here.
    fn legacy_home_fixture(work: &Path) -> PathBuf {
        #[derive(Serialize)]
        struct LegacyPointer {
            root: PathBuf,
        }
        let home = crate::mux::state_root().join(crate::mux::project_label(work));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(BINDING_FILE),
            toml::to_string(&LegacyPointer {
                root: work.canonical().unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        home
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

    /// The migration itself: a 0.2 home with real state in it moves onto
    /// the fresh id `bind` mints for the checkout, and the moved home
    /// carries the upgraded record — an `id` alongside the `root` a legacy
    /// `project.toml` never had.
    #[test]
    fn migrate_legacy_home_moves_a_02_home_onto_its_fresh_id() {
        let work = bind_fixture("legacy-move");
        let home = scratch_home("legacy-move");
        let legacy = crate::platform::test_home::with_home(&home, || {
            let legacy = legacy_home_fixture(&work);
            std::fs::create_dir_all(legacy.join("queue")).unwrap();
            std::fs::write(legacy.join("queue").join("t-1.md"), "task\n").unwrap();
            legacy
        });

        let moved = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("a 0.2 home with no live dispatcher and no live worktree moves");

        assert_ne!(moved, legacy, "moved onto a fresh, id-keyed home");
        assert!(!legacy.exists(), "nothing is left behind at the old path");
        assert!(
            moved.join("queue").join("t-1.md").is_file(),
            "the queue rode along with the move"
        );
        let binding: Binding =
            toml::from_str(&std::fs::read_to_string(moved.join(BINDING_FILE)).unwrap()).unwrap();
        assert_eq!(binding.root, work.canonical().unwrap());
        assert!(
            work.join(".git").join("spoolway-id").is_file(),
            "the checkout is stamped as part of the migration"
        );

        // Idempotent: a second command finds the checkout already stamped
        // and the migration already done, and changes nothing further.
        let again = crate::platform::test_home::with_home(&home, || bind(&work)).unwrap();
        assert_eq!(again, moved, "no later command moves anything again");
    }

    /// Acceptance criterion 3: a real linked worktree cut under the legacy
    /// home still resolves from the main checkout after the move.
    ///
    /// The whole point of the `git worktree repair` pass, and the one part
    /// of the migration a plain directory rename cannot carry on its own:
    /// git records a worktree's absolute path in the *main* checkout's own
    /// bookkeeping, under `.git/worktrees/<name>/gitdir`, and nothing about
    /// renaming the directory that path points into updates it. Asserted
    /// from the main checkout's own `git worktree list`, not from the moved
    /// worktree's side — the linked `.git` file there points back at a
    /// gitdir that never moved, so that side can look healthy while the
    /// record naming it is still stale.
    #[test]
    fn migrate_legacy_home_repairs_a_real_worktree_it_carried_across() {
        let work = bind_fixture("legacy-repair");
        std::fs::write(work.join("seed"), "seed\n").unwrap();
        git(&work, &["add", "seed"]);
        git(&work, &["commit", "-qm", "seed"]);

        let home = scratch_home("legacy-repair");
        let legacy = crate::platform::test_home::with_home(&home, || {
            let legacy = legacy_home_fixture(&work);
            std::fs::create_dir_all(legacy.join("worktrees")).unwrap();
            git(
                &work,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "task/lane",
                    &legacy.join("worktrees").join("lane").display().to_string(),
                ],
            );
            legacy
        });
        assert!(
            slashed(git(&work, &["worktree", "list"]))
                .contains(&slashed(legacy.display().to_string())),
            "the fixture's own worktree is recorded under the legacy home to begin with"
        );

        let moved = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("a legacy home with no live dispatcher and no live worktree moves");

        let new_worktree = moved.join("worktrees").join("lane");
        assert!(
            new_worktree.is_dir(),
            "the worktree rode along with the move"
        );
        let listed = git(&work, &["worktree", "list"]);
        assert!(
            slashed(&listed).contains(&slashed(new_worktree.display().to_string())),
            "the main checkout resolves the worktree at its new path; got:\n{listed}"
        );
        // The legacy *home* path is a prefix of the migrated one (the id
        // is simply appended to it), so the old worktree's own full path
        // is what distinguishes a stale record from a repaired one.
        assert!(
            !slashed(&listed).contains(&slashed(
                legacy.join("worktrees").join("lane").display().to_string()
            )),
            "and no longer names the old one; got:\n{listed}"
        );
    }

    /// Acceptance criterion 2: a live dispatcher over the legacy home
    /// blocks the move, naming the old path in full and leaving it
    /// untouched — and the same command succeeds once the run has
    /// finished.
    #[test]
    fn migrate_legacy_home_refuses_while_a_dispatcher_is_live_then_succeeds_once_it_stops() {
        let work = bind_fixture("legacy-live-dispatcher");
        let home = scratch_home("legacy-live-dispatcher");
        let legacy = crate::platform::test_home::with_home(&home, || legacy_home_fixture(&work));

        let lock_path = legacy.join(crate::lock::LOCK_FILE);
        let lock = crate::lock::Lock::acquire(&lock_path, false).unwrap();

        let err = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect_err("a live dispatcher over the legacy home refuses the move");
        let said = format!("{err:#}");
        assert!(said.contains("cannot move while work is live"), "{said}");
        assert!(said.contains("dispatcher running"), "{said}");
        assert!(
            said.contains(&legacy.display().to_string()),
            "names the old path in full: {said}"
        );
        assert!(
            legacy.join(BINDING_FILE).is_file(),
            "the old home is untouched"
        );
        assert!(
            !work.join(".git").join("spoolway-id").is_file(),
            "a refused migration must not stamp the checkout — otherwise the \
             next command lands on `bind_stamped`'s own \"no home holds the \
             id\" bail instead of retrying"
        );

        drop(lock);
        let moved = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("the same command succeeds once the dispatch has finished");
        assert!(!legacy.exists());
        assert!(moved.join(BINDING_FILE).is_file());
    }

    /// Acceptance criterion 2's other half: this very command running from
    /// inside one of the legacy home's own worktrees blocks the move just
    /// as a live dispatcher does, even with no dispatcher lock at all.
    #[test]
    fn migrate_legacy_home_refuses_while_a_lane_is_still_working_in_one_of_its_worktrees() {
        let work = bind_fixture("legacy-live-lane");
        let home = scratch_home("legacy-live-lane");
        let legacy = crate::platform::test_home::with_home(&home, || legacy_home_fixture(&work));
        let lane_wt = legacy.join("worktrees").join("task-a");
        std::fs::create_dir_all(&lane_wt).unwrap();

        // A lane record shaped exactly as `Headless` itself writes one,
        // naming this test's own pid — indisputably alive for as long as
        // the test runs, the same property `lock::tests` leans on for
        // `Lock::holder`. No `.exit` file: an unfinished turn is exactly
        // what `Headless::status` (and so `lane_working_under`) reads as
        // still working.
        let lane_dir = legacy.join(crate::headless::LANE_DIR);
        std::fs::create_dir_all(&lane_dir).unwrap();
        std::fs::write(
            lane_dir.join("task-a.json"),
            format!(
                r#"{{"name":"task-a","kind":"worktree","pane_id":"p","workspace_id":"w",
                     "tab_id":"t","cwd":{},"args":[],"env":{{}},"path_prefix":null,"turns":1}}"#,
                // Serialised, not dropped between quotes — see the same
                // fixture in `crate::headless`'s own tests for why a raw
                // Windows path cannot go inside a JSON string literal.
                serde_json::to_string(&lane_wt.display().to_string()).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(lane_dir.join("task-a.pid"), std::process::id().to_string()).unwrap();

        let err = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect_err("a lane still working in a legacy worktree refuses the move");
        let said = format!("{err:#}");
        assert!(said.contains("cannot move while work is live"), "{said}");
        assert!(said.contains("checked out"), "{said}");
        assert!(
            legacy.join(BINDING_FILE).is_file(),
            "the old home is untouched"
        );
        assert!(!work.join(".git").join("spoolway-id").is_file());

        // Once the lane has finished — an exit file, the same signal
        // `Headless::status` itself reads first — the same worktree no
        // longer blocks the move: acceptance criterion 3, only a worktree a
        // lane is still actually working in blocks it, and every other one
        // just rides along.
        std::fs::write(lane_dir.join("task-a.exit"), "0").unwrap();
        let moved = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect("no live dispatcher and no lane still working — free to move");
        assert!(!legacy.exists());
        assert!(moved.join(BINDING_FILE).is_file());
    }

    /// A checkout moved to a different *parent* directory, its own
    /// basename unchanged, still finds the legacy home its old basename
    /// keyed on — but that home's own recorded root now names the old
    /// path, not this one, so `bind` refuses rather than silently taking
    /// over a home that might still genuinely belong to the checkout that
    /// used to be at that recorded path. Not the `migrate-legacy-home`
    /// non-goal below — this one *is* found, by the unchanged basename —
    /// see `legacy_home_for`'s own doc for why the two are different.
    #[test]
    fn bind_refuses_a_legacy_home_at_this_basename_recording_a_different_root() {
        let work = bind_fixture("legacy-different-root");
        let home = scratch_home("legacy-different-root");
        let other_root = bind_fixture("legacy-different-root-other");
        crate::platform::test_home::with_home(&home, || {
            let legacy = legacy_home_fixture(&other_root);
            // `legacy_home_fixture` names the home after `other_root`'s own
            // basename, not `work`'s — moved here so it sits exactly where
            // `work`'s own basename would look for one, the collision this
            // proves against.
            let collision = crate::mux::state_root().join(crate::mux::project_label(&work));
            if legacy != collision {
                std::fs::rename(&legacy, &collision).unwrap();
            }
        });

        let err = crate::platform::test_home::with_home(&home, || bind(&work)).expect_err(
            "a legacy home at this basename recording a different root must not be guessed past",
        );
        let said = format!("{err:#}");
        assert!(
            said.contains("already holds a 0.2 home for a different checkout"),
            "{said}"
        );
        assert!(said.contains("--adopt"), "{said}");
        assert!(said.contains("--new-id"), "{said}");
        assert!(
            !work.join(".git").join("spoolway-id").is_file(),
            "refused rather than silently minting a fresh home instead"
        );
    }

    /// The `migrate-legacy-home` non-goal itself, proven rather than only
    /// asserted in prose: a checkout whose own *basename* changed along
    /// with its path leaves its legacy home behind at the old basename,
    /// unreachable from the new one — nothing on disk still links the two
    /// — so `bind` binds the checkout fresh, exactly as it would if no
    /// legacy home existed anywhere, and the old one is left for a person
    /// to `spoolway init --adopt` themselves. See `legacy_home_for`'s own
    /// doc for why this is undetectable rather than merely unhandled.
    ///
    /// Binding fresh here is the decided behaviour, not a gap. This is
    /// the same `bind_unstamped` Criterion 7 fallback every genuinely new
    /// checkout takes, and a renamed 0.2 checkout is byte-for-byte
    /// indistinguishable from a new one at this point — so the only ways
    /// to refuse here are to guess at a likely match (which the non-goal
    /// forbids in as many words) or to refuse *every* fresh clone on any
    /// machine that still has an orphaned 0.2 home lying around anywhere
    /// (which breaks `binding-record`'s own accepted Criterion 7, and the
    /// fresh-`init` output `tests/init_output.rs` pins line for line).
    ///
    /// What the non-goal actually requires is that `spoolway init --adopt`
    /// be the answer, and it now genuinely is one: `adopt` carries a
    /// legacy home onto a renamed checkout by hand — see
    /// `adopt_carries_a_legacy_home_onto_a_checkout_renamed_since_0_2`,
    /// which walks this exact rename through to recovery. Before that it
    /// did not work at all on a 0.2 home, which is what made this look
    /// like an unresolvable contradiction rather than a missing feature:
    /// every refusal in this area named a remedy that failed with a raw
    /// TOML "missing field `id`". `docs/installation.md`'s upgrade note
    /// tells a person coming from 0.2 to run it.
    #[test]
    fn bind_mints_a_fresh_home_when_this_checkouts_own_basename_has_changed_since_0_2() {
        let base = crate::scratch::root("bind-legacy-renamed-basename");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let before = base.join("api");
        std::fs::create_dir_all(&before).unwrap();
        crate::scratch::git_init(&before, &["-b", "plan/demo"]);
        let home = scratch_home("legacy-renamed-basename");

        let legacy_before =
            crate::platform::test_home::with_home(&home, || legacy_home_fixture(&before));
        assert!(legacy_before.ends_with("api"), "{legacy_before:?}");

        // The rename itself: same checkout, new basename. Its old legacy
        // home at `~/.spoolway/api/` is left exactly where it was — nothing
        // here ever deletes it — but nothing after this point ever looks
        // there again either.
        let after = base.join("billing");
        std::fs::rename(&before, &after).unwrap();

        let bound = crate::platform::test_home::with_home(&home, || bind(&after))
            .expect("a renamed checkout with no way back to its old home binds itself fresh");
        assert_ne!(
            bound, legacy_before,
            "a fresh home, not the old one this checkout can no longer be linked to"
        );
        assert!(bound.join(BINDING_FILE).is_file());
        assert!(after.join(".git").join("spoolway-id").is_file());
        assert!(
            legacy_before.is_dir(),
            "the old, now-unreachable legacy home is untouched, not deleted"
        );
    }

    /// `spoolway init --adopt <name>` is the answer the
    /// `migrate-legacy-home` non-goal names for a checkout renamed under
    /// 0.2, and every refusal in this area names it too — so it has to
    /// work on the shape it is pointed at. It did not: a 0.2
    /// `project.toml` carries a `root` and no `id`, and `read_binding`
    /// treats a missing `id` as a hard parse error, so `--adopt` on a
    /// legacy home failed with a raw TOML "missing field `id`" instead of
    /// adopting anything.
    ///
    /// The full non-goal route, end to end: a 0.2 checkout at `api` is
    /// renamed to `billing`, `bind` can no longer find its old home (the
    /// test above proves that, and proves it binds fresh), and the person
    /// who still remembers the old name recovers it by hand with the
    /// command the refusals told them to run.
    #[test]
    fn adopt_carries_a_legacy_home_onto_a_checkout_renamed_since_0_2() {
        let base = crate::scratch::root("adopt-legacy-renamed");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let before = base.join("api");
        std::fs::create_dir_all(&before).unwrap();
        crate::scratch::git_init(&before, &["-b", "plan/demo"]);
        let home = scratch_home("adopt-legacy-renamed");

        let legacy = crate::platform::test_home::with_home(&home, || {
            let legacy = legacy_home_fixture(&before);
            std::fs::create_dir_all(legacy.join("queue")).unwrap();
            std::fs::write(legacy.join("queue").join("t-1.md"), "task\n").unwrap();
            legacy
        });
        let name = legacy.file_name().unwrap().to_string_lossy().into_owned();

        // The rename `bind` can never see through.
        let after = base.join("billing");
        std::fs::rename(&before, &after).unwrap();

        let adopted = crate::platform::test_home::with_home(&home, || adopt(&after, &name))
            .expect("a person naming the old home by hand is the documented way back to it");

        assert!(
            adopted
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("billing-"),
            "the adopted home follows the checkout's current name, not the old one: {adopted:?}"
        );
        assert!(
            adopted.join("queue").join("t-1.md").is_file(),
            "the 0.2 queue this whole feature exists to rescue came across"
        );
        assert!(!legacy.exists(), "nothing is left behind at the old path");
        assert!(after.join(".git").join("spoolway-id").is_file());

        // And the very next ordinary command lands back on it by itself,
        // with no second adoption — the stamp `migrate_legacy_home` wrote
        // is what makes the recovery stick.
        let bound = crate::platform::test_home::with_home(&home, || bind(&after))
            .expect("the adopted home resolves on the next ordinary command");
        assert_eq!(bound, adopted);
    }

    /// Adopting a legacy home is the migration, asked for by hand — so it
    /// carries the migration's own refusals with it rather than becoming a
    /// way around them. A live dispatcher over the legacy home blocks
    /// `--adopt` exactly as it blocks the automatic route, and leaves the
    /// old home untouched to retry against.
    #[test]
    fn adopt_refuses_a_legacy_home_a_dispatcher_is_still_running_over() {
        let work = bind_fixture("adopt-legacy-live");
        let home = scratch_home("adopt-legacy-live");
        let legacy = crate::platform::test_home::with_home(&home, || legacy_home_fixture(&work));
        let name = legacy.file_name().unwrap().to_string_lossy().into_owned();

        let lock = crate::lock::Lock::acquire(&legacy.join(crate::lock::LOCK_FILE), false)
            .expect("a dispatcher's own lock over the legacy home");

        let err = crate::platform::test_home::with_home(&home, || adopt(&work, &name))
            .expect_err("adopting must not pull a home out from under a live dispatcher");
        let said = format!("{err:#}");
        assert!(said.contains("cannot move while work is live"), "{said}");
        assert!(
            said.contains(&legacy.display().to_string()),
            "names the old path in full: {said}"
        );
        assert!(legacy.join(BINDING_FILE).is_file(), "untouched");

        drop(lock);
        let adopted = crate::platform::test_home::with_home(&home, || adopt(&work, &name))
            .expect("the same command succeeds once the dispatch has finished");
        assert!(!legacy.exists());
        assert!(adopted.join(BINDING_FILE).is_file());
    }

    /// Acceptance criterion 4: a clone that already has a home settled
    /// under its id, and also still has a legacy home nobody ever moved
    /// (or moved back by hand), refuses and names both rather than
    /// merging or silently preferring one.
    #[test]
    fn bind_refuses_when_a_legacy_home_and_an_id_keyed_home_both_claim_one_checkout() {
        let work = bind_fixture("legacy-conflict");
        let home = scratch_home("legacy-conflict");
        let (id_home, legacy) = crate::platform::test_home::with_home(&home, || {
            let id_home = bind(&work).expect("binds itself with no legacy home in the way yet");
            let legacy = legacy_home_fixture(&work);
            (id_home, legacy)
        });

        let err = crate::platform::test_home::with_home(&home, || bind(&work))
            .expect_err("two homes claiming one checkout must not be merged or picked between");
        let said = format!("{err:#}");
        assert!(said.contains("two homes both claim"), "{said}");
        assert!(
            said.contains(&id_home.display().to_string()),
            "names the id-keyed home: {said}"
        );
        assert!(
            said.contains(&legacy.display().to_string()),
            "names the legacy home: {said}"
        );
        assert!(legacy.exists(), "neither home is touched by the refusal");
        assert!(id_home.exists());
    }

    /// Acceptance criterion 5: two commands racing to resolve the same
    /// project's home for the first time after an upgrade leave exactly one
    /// moved home and no error, whichever of them actually wins the rename.
    #[test]
    fn two_racing_first_resolvers_leave_exactly_one_moved_legacy_home() {
        let work = bind_fixture("legacy-race");
        let home = scratch_home("legacy-race");
        crate::platform::test_home::with_home(&home, || legacy_home_fixture(&work));

        let contenders = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(contenders));
        let handles: Vec<_> = (0..contenders)
            .map(|_| {
                let work = work.clone();
                let home = home.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    crate::platform::test_home::with_home(&home, || bind(&work))
                })
            })
            .collect();
        let results: Vec<Result<PathBuf>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();

        for result in &results {
            assert!(result.is_ok(), "no racer sees an error: {result:?}");
        }
        let resolved: std::collections::BTreeSet<PathBuf> =
            results.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(
            resolved.len(),
            1,
            "every racer agrees on the identical moved home"
        );
    }

    /// The scratch pair [`rename_onto_home`]'s own tests move around: a
    /// legacy home standing at `legacy`, and the id-keyed destination it is
    /// headed for, which may or may not exist yet.
    fn rename_pair(name: &str) -> (PathBuf, PathBuf) {
        let root = crate::scratch::root(&format!("rename-onto-home-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (root.join("legacy"), root.join("home"))
    }

    /// The migration this call did itself, reported as its own: the one
    /// answer that goes on to repair the moved worktrees and print the move.
    #[test]
    fn the_racer_that_moves_the_home_is_the_one_told_it_moved() {
        let (legacy, home) = rename_pair("winner");
        std::fs::create_dir_all(legacy.join("queue")).unwrap();

        assert!(rename_onto_home(&legacy, &home).unwrap());
        assert!(home.join("queue").is_dir(), "the home moved with its work");
        assert!(!legacy.exists(), "and nothing is left at the old path");
    }

    /// A lost race is read off the outcome — the legacy home gone with the
    /// id-keyed home standing — and not off the one errno Unix happens to
    /// report for it. This is the Windows failure in the form every
    /// platform can run: there the losing racer is told
    /// `ERROR_ACCESS_DENIED`, not the `ENOENT` this branch used to insist
    /// on, and the migration beside it had already succeeded either way.
    #[test]
    fn a_home_found_already_moved_reports_the_race_lost_not_an_error() {
        let (legacy, home) = rename_pair("loser");
        std::fs::create_dir_all(&home).unwrap();

        assert!(
            !rename_onto_home(&legacy, &home).unwrap(),
            "somebody else moved it; nothing left to do but agree"
        );
    }

    /// A rename that is genuinely stuck rather than raced still reports its
    /// own error once the patience is spent: the legacy home is still
    /// standing, so nothing has migrated, and pretending otherwise would
    /// write a binding for a home that never moved.
    ///
    /// A destination directory that already holds content of its own is the
    /// stuck shape both platforms agree on — `ENOTEMPTY` on Unix, and the
    /// `ERROR_ACCESS_DENIED` Windows reports for any rename onto a standing
    /// directory, waited out and then reported. A plain *file* at the
    /// destination is not that shape: Windows moves a directory straight
    /// over one, where Unix refuses.
    #[test]
    fn a_rename_that_is_stuck_rather_than_raced_still_fails() {
        let (legacy, home) = rename_pair("stuck");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(home.join("queue")).unwrap();

        let err = rename_onto_home(&legacy, &home).expect_err("the destination is occupied");
        let said = format!("{err:#}");
        assert!(
            said.contains(&format!("moving {}", legacy.display())),
            "the error names what it was moving: {said}"
        );
        assert!(legacy.is_dir(), "and leaves the old home where it stands");
    }

    /// [`contended`] answers for Windows on every platform, so both halves
    /// of the judgement are checked here rather than only the one this CI
    /// leg happens to build for.
    #[test]
    fn only_windows_reads_a_refused_rename_as_worth_waiting_on() {
        use std::io::{Error, ErrorKind};

        // `ERROR_ACCESS_DENIED`, which Rust maps to a kind of its own on
        // Windows, is the refusal the racing migration actually hit.
        let denied = Error::from(ErrorKind::PermissionDenied);
        // `ERROR_SHARING_VIOLATION`, which it maps to no kind, so the raw
        // number is the only way to name it. On Unix 32 is `EPIPE`, which
        // is precisely why `windows` gates the question at all.
        let sharing = Error::from_raw_os_error(32);
        let missing = Error::from(ErrorKind::NotFound);

        assert!(contended(&denied, true));
        assert!(
            !contended(&denied, false),
            "a Unix refusal means what it says"
        );
        assert!(contended(&sharing, true), "the other Win32 spelling of it");
        assert!(!contended(&sharing, false));
        assert!(
            !contended(&missing, true),
            "a missing source is not waiting"
        );
        assert!(!contended(&missing, false));
    }

    /// The scratch pair [`reread_record`]'s own tests work on: a home
    /// holding the record about to be looked at a second time, and the
    /// checkout it is being looked at on behalf of. Both are real paths,
    /// because both halves of that judgement are made off what is on disk.
    fn record_pair(name: &str) -> (PathBuf, PathBuf) {
        let root = crate::scratch::root(&format!("reread-record-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let (home, work) = (root.join("home"), root.join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        (home, work)
    }

    /// Writes the 0.2-era record shape — a `root`, no `id` — straight into
    /// `home`, the way an interrupted migration leaves it behind.
    fn legacy_record(home: &Path, root: &Path) {
        #[derive(Serialize)]
        struct LegacyPointer {
            root: PathBuf,
        }
        std::fs::write(
            home.join(BINDING_FILE),
            toml::to_string(&LegacyPointer {
                root: root.to_path_buf(),
            })
            .unwrap(),
        )
        .unwrap();
    }

    /// The Linux half of the racing migration, in the form every platform
    /// can run: a racer that read the legacy record, failed to parse it,
    /// and only then asked again — by which time the winner beside it had
    /// replaced that record with a whole `Binding`. The parse error it is
    /// still holding describes a file that no longer exists, so the record
    /// standing there now is the answer, not that error.
    #[test]
    fn a_record_a_racing_migration_upgraded_is_read_as_what_it_says_now() {
        let (home, work) = record_pair("superseded");
        legacy_record(&home, &work);
        // The winner lands between the two reads.
        write_binding(
            &home,
            &Binding {
                id: "beadedbeadedbead".to_string(),
                root: work.clone(),
            },
        )
        .unwrap();

        match reread_record(&home, &work) {
            Reread::Superseded(binding) => {
                assert_eq!(binding.id, "beadedbeadedbead");
                assert_eq!(binding.root, work);
            }
            other => panic!("a finished migration is not an error to report: {other:?}"),
        }
    }

    /// The case the arm was written for is untouched: a migration that
    /// moved the home and was killed before upgrading the record still
    /// reads as an upgrade to finish, not as a corrupt file.
    #[test]
    fn an_interrupted_legacy_upgrade_is_still_read_as_one() {
        let (home, work) = record_pair("interrupted");
        legacy_record(&home, &work);

        assert!(matches!(reread_record(&home, &work), Reread::LegacyUpgrade));
    }

    /// A record that is neither shape keeps the first read's error: nothing
    /// here guesses past a genuinely corrupt `project.toml`.
    #[test]
    fn a_record_that_is_neither_shape_stays_corrupt() {
        let (home, work) = record_pair("corrupt");
        std::fs::write(home.join(BINDING_FILE), "id = [not even toml\n").unwrap();

        assert!(matches!(reread_record(&home, &work), Reread::Corrupt));
    }

    /// A legacy record naming some *other* checkout is not this checkout's
    /// upgrade to finish, and the second read does not turn it into one —
    /// it parses as neither shape for this `root`, so the error stands.
    #[test]
    fn a_legacy_record_naming_another_checkout_is_not_this_ones_upgrade() {
        let (home, work) = record_pair("elsewhere");
        legacy_record(&home, &work.parent().unwrap().join("somebody-else"));

        assert!(matches!(reread_record(&home, &work), Reread::Corrupt));
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
        let (_origin, work) = fixture("branch-without-state");
        let home = scratch_home("branch-without-state");
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
        let (_origin, work) = fixture("stamp-format");
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
        let (_origin, work) = fixture("stamp-stable");
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
        let (_origin, work) = fixture("stamp-file");
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
    #[cfg(unix)]
    #[test]
    fn a_write_failure_is_an_error_not_an_absent_stamp() {
        use std::os::unix::fs::PermissionsExt;
        let (_origin, work) = fixture("stamp-write-failure");
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
        let (_origin, work) = fixture("stamp-race");
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
        let (origin, _work) = fixture("stamp-clone-origin");
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
        let (_origin, work) = fixture("label-traversal");
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
    /// `#[cfg(unix)]`: on Windows `\` really is a path separator, so
    /// `.join("api\\copy")` below would name two path components, not one
    /// unsafe basename, and the rename would fail against a directory that
    /// was never created. `sanitize_label_folds_an_unsafe_character_to_a_safe_one`
    /// below covers the same fix platform-independently, straight against
    /// `sanitize_label` rather than through a real rename.
    #[cfg(unix)]
    #[test]
    fn a_basename_that_is_not_a_safe_label_still_stamps_and_reads_back() {
        let base = crate::scratch::root("repo-test-unsafe-label");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        crate::scratch::git_init(&base, &["-q", "-b", "main"]);
        // Renamed to a basename `is_valid_label` refuses outright — a raw
        // `\`, legal in a Unix filename, is indistinguishable from a
        // Windows path separator to `is_bare_filename`.
        let weird = base.parent().unwrap().join("api\\copy");
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
}
