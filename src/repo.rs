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
    /// `~/.spoolway/<basename of root>/` — where every runtime file this
    /// project's spoolway writes actually lives. See [`Repo::home`] for why
    /// this is a field rather than a method computed from `root` on every
    /// call: a test fixture sets it to a scratch directory directly, so
    /// nothing under test ever depends on the real `$HOME`.
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
        let home = crate::mux::project_home(&root);
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
    /// Only `doctor` discovers this way. Every other command is right to die on
    /// a config it cannot parse — running on defaults would be running on
    /// settings the project never wrote. But `doctor` is the command you reach
    /// for *because* the config is wrong, so it takes the parse error as a
    /// finding rather than a reason not to start. The defaults it gets in place
    /// of a config are not reported on: they exist so there is a `Repo` to hang
    /// the checks that read no settings off of.
    pub fn discover_lenient(start: &Path) -> Result<(Repo, Option<anyhow::Error>)> {
        let start = start
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        let main = main_checkout(&start);
        let root = Repo::root(&start, main.as_deref())?;
        let checkout = checkout_of(&start, &root, main.as_deref());
        let home = crate::mux::project_home(&root);
        match Config::load(&root) {
            Ok(config) => Ok((
                Repo {
                    root,
                    checkout,
                    config,
                    home,
                },
                None,
            )),
            Err(err) => Ok((
                Repo {
                    root,
                    checkout,
                    config: Config::default(),
                    home,
                },
                Some(err),
            )),
        }
    }

    /// The project directory `start` belongs to, before anything is read out of
    /// it. `start` is already canonicalized, and `main` is `main_checkout`'s
    /// own answer for it — both callers resolve each once and share them with
    /// [`checkout_of`], so this makes no `git rev-parse` call that a caller
    /// has not already paid for.
    fn root(start: &Path, main: Option<&Path>) -> Result<PathBuf> {
        main.filter(|dir| dir.join(crate::config::STATE_DIR).is_dir())
            .map(Path::to_path_buf)
            .or_else(|| {
                start
                    .ancestors()
                    .find(|dir| dir.join(crate::config::STATE_DIR).is_dir())
                    .map(Path::to_path_buf)
            })
            .or_else(|| git_toplevel(start).ok())
            .with_context(|| {
                format!(
                    "no spoolway project found at or above {} (run `spoolway init` there first)",
                    start.display()
                )
            })
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

    /// `~/.spoolway/<basename of root>/` — every runtime file this project's
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

    /// Where a project overrides the six typed messages a lane's pane
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
    /// for longer than `retention.days` — and [`crate::tracking::failure_count`],
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
    /// A dependency whose task cannot be loaded is an error that names the
    /// dependency, not a `task/<dep_id>` guess handed on to a cut that then
    /// fails over a ref nobody can place.
    pub fn dependency_branch(&self, dep_id: &str) -> Result<String> {
        // The wrapped error from [`Repo::task`] already says whether the file
        // is missing or fails to parse; this only adds why it matters and
        // what to do, without asserting which of the two it was.
        let dep = self.task(dep_id).with_context(|| {
            format!(
                "The branch of dependency `{dep_id}` could not be resolved. Add its task file \
                 to this project's queue or archive, or repair it if it is already there, \
                 before running a task that depends on it."
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
    let common = run(
        dir,
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let common = PathBuf::from(common.trim());
    // A bare repo has no checkout to speak of.
    if common.file_name()? != "git" && !common.ends_with(".git") {
        return None;
    }
    // Canonicalized so it compares against the `start` every caller has
    // already canonicalized. On Windows the two spellings otherwise never
    // match: git prints `C:/Users/…` while `canonicalize` answers verbatim
    // `\\?\C:\…`, whose prefix component is a different thing — and with
    // that mismatch every main checkout read as a linked worktree.
    common.parent()?.canonicalize().ok()
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
        let repo = Repo::discover(&work).unwrap();
        let dirs = repo.byproduct_dirs();

        assert!(
            !dirs.contains(&repo.prompts_dir()),
            "the checkout's tracked .spoolway/prompts/ is swept: {dirs:?}"
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

        let repo = Repo::discover(&wt).unwrap();
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

        let repo = Repo::discover(&wt).unwrap();
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
        let repo = Repo::discover(&work).unwrap();
        assert!(repo.checkout_note().unwrap().is_none());
    }

    #[test]
    fn discover_from_the_project_itself_is_unchanged() {
        let (_origin, work) = fixture("plain");
        let repo = Repo::discover(&work).unwrap();
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

        assert!(Repo::discover(&work).is_err(), "every other command dies");

        let (repo, err) = Repo::discover_lenient(&work).unwrap();
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

    /// Two plans in flight at once: each has a branch and a worktree of its
    /// own, and one dispatcher serves both. So a branch being looked up is
    /// usually not the one the dispatcher's own checkout is on.
    #[test]
    fn a_branch_checked_out_in_another_worktree_is_found_there() {
        let (_origin, work) = fixture("worktree-lookup");
        let repo = Repo::discover(&work).unwrap();

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

    #[test]
    fn a_repo_with_no_remote_reports_so() {
        let base = crate::scratch::root("repo-test-noremote");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(crate::config::STATE_DIR)).unwrap();
        git(&base, &["init", "-q", "-b", "work"]);

        let repo = Repo::discover(&base).unwrap();
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

        let repo = Repo::discover(&sub).unwrap();
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
        std::fs::remove_dir_all(&base).ok();
    }
}
