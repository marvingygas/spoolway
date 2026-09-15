//! Locating the repo spoolway is operating on, and the thin git wrapper every
//! other module goes through.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Config;
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
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        let main = main_checkout(&start);
        let root = Repo::root(&start, main.as_deref())?;
        let checkout = checkout_of(&start, &root, main.as_deref());
        let config = Config::load(&root)?;
        let home = crate::mux::project_home(&root)?;
        // Refused here, once, rather than in every accessor that creates a
        // directory under `home` on demand: a project nobody has run `init`
        // in has no claim on `~/.spoolway/<name>/`, and a command that went
        // on regardless would create that directory — queue, archive and
        // all — for a checkout the name may not even belong to. `doctor`
        // takes the same fact as a finding through `discover_lenient`, and a
        // test fixture that sets `home` by hand never comes through here.
        if !crate::commands::registered(&home) {
            bail!(
                "{} is not registered as a spoolway project: {} does not exist. Run \
                 `spoolway init` in {} first, so that this checkout claims its state \
                 directory before anything is written there.",
                root.display(),
                home.join(crate::commands::PROJECT_FILE).display(),
                root.display(),
            );
        }
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
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        let main = main_checkout(&start);
        let root = Repo::root(&start, main.as_deref())?;
        let checkout = checkout_of(&start, &root, main.as_deref());
        let (home, home_error) = crate::mux::project_home_lenient(&root);
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
                .and_then(|home| crate::commands::pointer_root(&home))
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
    /// Created silently — a fresh clone, or a home directory deleted by hand,
    /// gets one back the moment anything is asked to resolve under it, rather
    /// than failing or printing a note nobody asked for: an empty queue is the
    /// honest answer for a machine that has run nothing yet.
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

    /// Where a project describes what belongs under each heading spoolway
    /// appends to a task file — see [`crate::task_log`].
    pub fn task_log_path(&self) -> PathBuf {
        self.checkout.join(crate::config::TASK_LOG_TEMPLATE)
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
/// `pub(crate)` rather than private: the herdr backend calls this a second
/// time, on [`Repo::root`] itself, to resolve the `--cwd` it gives `worktree
/// open` — see [`crate::mux::Herdr::new`]. `Repo::root` usually already names
/// the main checkout, but a project whose `.spoolway/` sits inside a linked
/// worktree finds that worktree first, through [`Repo::root`]'s own ancestor
/// search, and herdr refuses a `--cwd` that is itself a linked worktree with
/// `linked_worktree_source`.
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
        .canonicalize()
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
/// rather than falling back to a basename-keyed home. This is
/// [`commands::init::init`]'s call alone — every other reader of a
/// project's identity goes through [`project_identity`], which only ever
/// reads what this has already written, and mints nothing: a plain `git
/// clone` carries no `.git/spoolway-id` of its own (it is not a tracked
/// file), so a second clone stays unresolvable, not silently self-stamped,
/// until someone actually runs `spoolway init` in it.
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
/// matching what every caller always got before this stamp existed. This is
/// deliberate and load-bearing, not a shortcut: every reader that is not
/// `spoolway init` itself — `Repo::discover`'s own "is this checkout
/// registered under a different name" nicety included — must never *create*
/// a stamp merely by asking about one, or running any ordinary command
/// against an unrelated git repository would silently write into its
/// `.git`.
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

fn is_valid_id(candidate: &str) -> bool {
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
    dir.canonicalize().unwrap_or(dir)
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
    top.canonicalize()
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

    /// A scratch `$HOME`, canonicalized the way `Repo::root` canonicalizes
    /// the path it walks up from, so a `.spoolway` planted under it compares
    /// equal to what discovery sees.
    fn scratch_home(name: &str) -> PathBuf {
        let home = crate::scratch::root(&format!("repo-test-home-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home.canonicalize().unwrap()
    }

    /// `Repo::discover`, on a checkout `spoolway init` has claimed under a
    /// scratch home of its own — the one registration discovery insists on,
    /// written the way `init` writes it, and never into the real
    /// `~/.spoolway/`.
    fn discover_registered(work: &Path) -> Result<Repo> {
        discover_registered_as(work, work)
    }

    /// The same, started from `start` — a linked worktree of `project`, in
    /// the tests that need one — with `project` being what `init` claimed.
    fn discover_registered_as(project: &Path, start: &Path) -> Result<Repo> {
        let home = scratch_home("registered");
        crate::platform::test_home::with_home(&home, || {
            crate::commands::claim(project, false).unwrap();
            Repo::discover(start)
        })
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
            repo.root.canonicalize().unwrap(),
            work.canonicalize().unwrap(),
            "a lane running in its worktree must resolve to the project"
        );
        assert_eq!(
            repo.checkout.canonicalize().unwrap(),
            wt.canonicalize().unwrap(),
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
            main_dir.canonicalize().unwrap(),
            work.join(".git").canonicalize().unwrap()
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
        assert_eq!(
            note.path.canonicalize().unwrap(),
            wt.canonicalize().unwrap()
        );
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
            repo.checkout.canonicalize().unwrap(),
            repo.root.canonicalize().unwrap(),
            "in the main checkout, checkout and root are the same directory"
        );
        assert_eq!(
            repo.root.canonicalize().unwrap(),
            work.canonicalize().unwrap()
        );
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
            crate::commands::claim(&work, false).unwrap();
            (Repo::discover(&work), Repo::discover_lenient(&work))
        });
        assert!(strict.is_err(), "every other command dies");

        let (repo, err, home_error) = lenient.unwrap();
        assert!(home_error.is_none(), "the home itself resolved fine here");
        assert_eq!(
            repo.root.canonicalize().unwrap(),
            work.canonicalize().unwrap()
        );
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
                .canonicalize()
                .unwrap(),
            plan_y.canonicalize().unwrap(),
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
            repo.root.canonicalize().unwrap(),
            sub.canonicalize().unwrap(),
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

    /// The registration `init` writes is what every other command insists
    /// on: a `.spoolway/` in the checkout is not enough, because every
    /// accessor under `Repo` creates its home on demand, and a home nobody
    /// claimed is how `~/.spoolway/<name>/` was conjured up for the wrong
    /// checkout. `doctor` still gets a `Repo` to report the fact with.
    #[test]
    fn discovery_refuses_a_project_init_never_registered() {
        let (_origin, work) = fixture("unregistered");
        let home = scratch_home("unregistered");

        let (strict, lenient) = crate::platform::test_home::with_home(&home, || {
            (Repo::discover(&work), Repo::discover_lenient(&work))
        });
        let err = strict.expect_err("a project nobody ran `init` in is refused");
        let said = format!("{err:#}");
        assert!(
            said.contains("not registered") && said.contains("`spoolway init` in"),
            "the error says what to do: {said}"
        );
        assert!(
            !home.join(crate::config::STATE_DIR).exists(),
            "refusing must not create the very directory it refuses to claim"
        );

        let (repo, config_error, home_error) = lenient.unwrap();
        assert!(config_error.is_none());
        assert!(home_error.is_none());
        assert_eq!(
            repo.root.canonicalize().unwrap(),
            work.canonicalize().unwrap()
        );
        assert!(
            !crate::commands::registered(&repo.home),
            "doctor's own Repo carries the unregistered home, to report on"
        );
    }

    /// `.spoolway/` is tracked, so a checkout is a project on one branch
    /// and not on another. A registered project on a branch that lacks it
    /// is told exactly that, by branch name, rather than the generic
    /// not-found error a never-initialised directory gets.
    #[test]
    fn a_registered_project_on_a_branch_without_its_state_dir_is_told_so() {
        let (_origin, work) = fixture("branch-without-state");
        let home = scratch_home("branch-without-state");
        let err = crate::platform::test_home::with_home(&home, || {
            crate::commands::claim(&work, false).unwrap();
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
        assert_eq!(checkout, work.canonicalize().unwrap());
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
