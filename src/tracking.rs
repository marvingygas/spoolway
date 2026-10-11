//! The `[issue_tracking]` hook: one script a project names, run on seven
//! events. Four are stages a task can come to rest on — `queued`, `blocked`,
//! `paused` and `done` — see `crate::pipeline::RESERVED`, which lists exactly
//! them: they are the states nothing inside a pipeline file can already put a
//! `run:` step on, since none of them is a step a pipeline may declare. The
//! other three are not task stages at all: `open`, which [`open_ticket`] runs
//! synchronously from `queue add` itself, before any of the four could ever
//! fire; `fetch`, which [`fetch_issue`] runs synchronously from `spoolway
//! issue show`, before any task tied to the issue it names even exists; and
//! `started`, which [`fire`] runs once a task actually leaves `queued` for
//! its entry step — see `crate::pipeline::STARTED` for why this is not a
//! fifth reserved stage, only ever fired and gated from inside the `queued`
//! arm of `crate::dispatch::Dispatcher::route_reserved_stage`. A hook proves
//! itself on these real events alone — there is no `check` event asking it
//! to prove itself ahead of time; a failing event already pauses the task
//! with the reason named (see `crate::dispatch::Dispatcher::tracking_gate`).
//!
//! This reuses [`crate::command_step::Runs`] rather than reinventing a second
//! way to spawn something detached and read its exit code back on a later
//! pass — a hook run is the same shape as a `run:` step's. It gets its own
//! directory, `tracking/` rather than `commands/`, so a project checking on a
//! stuck ticket integration never has to sift through build logs to find it.
//!
//! Firing is idempotent for a run's whole lifetime: [`fire`] starts a run
//! only while [`RunState::Fresh`] holds. `queued`, `paused` and `blocked`
//! are stages a task can sit on for many passes in a row, which is exactly
//! what that guards: without it every pass sitting on `blocked` would open a
//! second ticket. The one exception is a run killed without an exit code,
//! [`RunState::Interrupted`]: [`fire`] starts it again, up to
//! [`MAX_HOOK_KILLS`] runs in a row, so a hook that did open a ticket before
//! a reboot killed it may open a second one. The one caller that ever calls
//! [`Runs::forget`] is [`forget`] — a non-zero exit on `queued`, `started` or
//! `done` pauses the task (see `crate::dispatch::Dispatcher::tracking_gate`), and
//! [`forget`] is what `spoolway resume` calls to undo that hold, so the very
//! next pass's [`fire`] starts the hook over rather than reading the same
//! stale exit code forever. [`open_ticket`] needs none of that: `queue add`
//! is the only caller there ever is, and it means "run this now" every time
//! it calls at all — a task already naming a `ticket:` is what its own
//! caller, `queue::open_tickets`, reads as "already open" and skips before
//! this is ever reached.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::command_step::{RunState, Runs};
use crate::repo::Repo;
use crate::task::Task;

/// Directory under the project's home holding one hook run's pid, exit code
/// and log per task per event — see [`crate::repo::Repo::tracking_dir`].
pub const TRACKING_DIR: &str = "tracking";

fn runs(repo: &Repo) -> Runs {
    Runs::new(&repo.tracking_dir())
}

/// The hook script this project has named, resolved inside `.spoolway/hooks/`
/// in the checkout — never a path, so a hook can only ever name a script this
/// project ships in that one directory. `None` when `hook` is blank (no issue
/// tracking) or when it is not a bare filename at all — see
/// [`is_bare_filename`] — rather than joining it onto the hooks directory and
/// trusting whatever comes out.
fn hook_path(repo: &Repo) -> Option<PathBuf> {
    hook_path_in(&repo.checkout, &repo.config.issue_tracking.hook)
}

/// [`hook_path`], against a checkout and a hook name directly rather than a
/// [`Repo`] — what `doctor` needs, since it checks its own freshly loaded
/// [`crate::config::Config`] rather than `repo.config`.
pub fn hook_path_in(checkout: &Path, hook: &str) -> Option<PathBuf> {
    let named = hook.trim();
    if named.is_empty() || !is_bare_filename(named) {
        return None;
    }
    Some(hooks_dir_in(checkout).join(named))
}

/// `.spoolway/hooks/` under `checkout` — the one place that subdirectory is
/// ever joined onto one. Every reader of a specific hook script goes through
/// [`hook_path_in`], which joins onto this rather than repeating the literal.
pub(crate) fn hooks_dir_in(checkout: &Path) -> PathBuf {
    crate::config::setup_dir_in(checkout).join("hooks")
}

/// Whether `name` is exactly one ordinary path component, which is the whole
/// of what makes `hook_path`'s join safe. A `hook` reaching here from a
/// hand-edited `config.toml` is not `spoolway config set`'s to have validated
/// first, so this is checked again at the one place the value is actually
/// turned into a path rather than trusted because of where it came from.
/// `spoolway doctor` calls this too, so a project naming something else finds
/// out without a hook ever silently failing to run.
///
/// Two checks, not one. The separator scan keeps `a\b.sh` out on every
/// platform, since `\` is not a path separator off Windows and
/// [`std::path::Component`] would read the whole thing as one `Normal`
/// component. The component check is what a scan for `/`, `\`, `.` and `..`
/// misses: `C:evil.ps1`, which `Path::join` on Windows resolves as
/// drive-relative outside the hooks directory (review finding 56). A single
/// `Component::Normal` is none of those — no separator, no `.`/`..`, no drive
/// or root prefix.
pub(crate) fn is_bare_filename(name: &str) -> bool {
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    let mut components = std::path::Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// Why `hook` can never run, or `None` when it is blank or a bare filename.
///
/// What `spoolway doctor` fails on and what `spoolway config set` refuses, so
/// the two say the same thing. The text starts at the verb, for each caller to
/// put its own subject in front of.
pub(crate) fn not_bare_filename_problem(hook: &str) -> Option<String> {
    let hook = hook.trim();
    (!hook.is_empty() && !is_bare_filename(hook)).then(|| {
        format!(
            "names `{hook}`, which is not a bare filename — a hook only ever runs a script \
             inside .spoolway/hooks/, so this can never run. Use a bare name, or clear hook to \
             switch issue tracking off."
        )
    })
}

/// What one synchronous `open` hook call came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenResult {
    /// No hook is configured at all — `queue add` runs no process, and
    /// whatever the caller already had for `epic`/`ticket` is unchanged.
    NoHook,
    /// The hook exited zero and answered. `slug` and `url` are two more
    /// optional lines it may add; a hook writing neither reads back blank,
    /// exactly as a missing `epic=` does. All four are opaque here — `queue
    /// add` does the checking:
    ///
    /// - `slug` feeds naming only, so it is consulted only when
    ///   `issue_tracking.key_in_names` is on, and dropped unless it passes
    ///   [`crate::config::check_id`]'s alphabet and is at most `SLUG_MAX_LEN`
    ///   characters (see `accept_slug` in `commands::queue`).
    /// - `url` is stored on the task whatever the flag says — it is there for
    ///   `terminal-names` to use later — and dropped unless it is an absolute
    ///   `http`/`https` address.
    ///
    /// A failing check is reported and the value dropped; the batch is never
    /// refused over one.
    Answered {
        epic: String,
        ticket: String,
        slug: String,
        url: String,
    },
    /// The hook exited non-zero, or ended without a code at all — the same
    /// [`RunState::Interrupted`] a command step's own run can end in.
    Failed { exit_code: Option<i32> },
}

/// Run the `open` hook for one task, synchronously, and return what it said.
///
/// Unlike [`fire`] this never checks [`RunState::Fresh`] first and never
/// leaves a process running past this call: `queue add` is the only caller,
/// it always means "run this now", and it needs the answer before it can
/// decide whether to queue anything at all — so this starts the run and
/// blocks on it rather than firing detached and leaving a later pass to
/// notice. `group_size` and `group_epic` become `SPOOLWAY_GROUP_SIZE` and
/// `SPOOLWAY_EPIC`; `depends_tickets` becomes `SPOOLWAY_DEPENDS_TICKETS`,
/// already resolved by the caller — see `queue::open_tickets`, which is what
/// walks a submission in the order that makes that possible at all.
pub fn open_ticket(
    repo: &Repo,
    task: &Task,
    group_size: usize,
    group_epic: &str,
    depends_tickets: &str,
    group_description: &str,
    task_file: &str,
) -> Result<OpenResult> {
    let Some(hook) = hook_path(repo) else {
        return Ok(OpenResult::NoHook);
    };

    let dir = repo.tracking_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = Runs::key("open", task.id());
    let out_path = dir.join(format!("{key}.out"));
    let _ = std::fs::remove_file(&out_path);

    let mut env = open_env(
        repo,
        task,
        group_size,
        group_epic,
        depends_tickets,
        group_description,
        task_file,
    );
    env.insert("SPOOLWAY_OUT".to_string(), out_path.display().to_string());

    let run_line = crate::platform::quote(&hook.display().to_string());
    let runs = runs(repo);
    runs.start(&key, &run_line, &repo.root, &env)?;

    // `queue add` is a one-shot command, not a dispatch pass — there is no
    // later turn to come back and check this on, so this blocks until the
    // hook is actually done rather than answering `Fresh`/`Running` back to
    // a caller with nothing useful to do with either.
    let state = loop {
        match runs.state(&key) {
            RunState::Fresh | RunState::Running => {
                std::thread::sleep(Duration::from_millis(25));
            }
            settled => break settled,
        }
    };

    Ok(match state {
        RunState::Exited(0) => {
            let Answer {
                epic,
                ticket,
                slug,
                url,
            } = read_answer(&out_path);
            OpenResult::Answered {
                epic,
                ticket,
                slug,
                url,
            }
        }
        RunState::Exited(code) => OpenResult::Failed {
            exit_code: Some(code),
        },
        RunState::Interrupted => OpenResult::Failed { exit_code: None },
        RunState::Fresh | RunState::Running => unreachable!("the loop above only exits settled"),
    })
}

/// Every variable every event carries — `spoolway hook contract` prints this
/// table first, before any event's own.
pub(crate) const COMMON_EVENT_VARS: &[(&str, &str)] = &[
    ("SPOOLWAY_EVENT", "which of the seven events this run is"),
    (
        "SPOOLWAY_PROJECT_KEY",
        "`issue_tracking.project_key`, verbatim, opaque to spoolway",
    ),
];

/// Every variable [`open_env`] adds beyond [`COMMON_EVENT_VARS`], plus the
/// three [`open_ticket`] inserts itself once `open_env`'s own map comes back
/// — kept beside the function that builds them rather than copied into
/// `commands::hook`, so `spoolway hook contract` renders this table instead
/// of a second copy of it.
/// `tests::open_and_dispatch_vars_match_a_real_environment` is what checks
/// the two never drift apart.
pub(crate) const OPEN_EVENT_VARS: &[(&str, &str)] = &[
    ("SPOOLWAY_TASK", "the id it is about to be queued under"),
    ("SPOOLWAY_SOURCE", "the task's own `source:`"),
    ("SPOOLWAY_GROUP", "its `group:`"),
    (
        "SPOOLWAY_GROUP_DESCRIPTION",
        "its group's own `group_description:`, if one was set",
    ),
    ("SPOOLWAY_BRANCH", "its `branch:`"),
    ("SPOOLWAY_TITLE", "its title"),
    (
        "SPOOLWAY_TASK_FILE",
        "the task's path while this hook runs; blank when it has no file of its own",
    ),
    (
        "SPOOLWAY_GROUP_SIZE",
        "how many tasks this group is opening at once",
    ),
    (
        "SPOOLWAY_LABELS",
        "its `labels:`, comma-joined, empty when it has none",
    ),
    (
        "SPOOLWAY_EPIC",
        "the group's already-open epic, if this is not its first task",
    ),
    (
        "SPOOLWAY_DEPENDS_TICKETS",
        "tickets of the tasks this one's `depends_on:` names",
    ),
    (
        "SPOOLWAY_OUT",
        "where to write the answer: `epic=`, `ticket=`, `slug=` and `url=` lines, any \
         order, all optional",
    ),
];

/// The environment [`open_ticket`] runs its hook with — everything
/// [`build_env`] gives the four dispatch events that also makes sense before
/// a task has ever moved: no `SPOOLWAY_FROM`, since nothing has happened to
/// this task yet to name.
///
/// `task_file` is handed in rather than read off `task.path`: at the moment
/// this hook runs the task has not been written to the queue yet — `task.
/// path` already names where `validate_batch` intends to save it, a file
/// that does not exist until after every task in the batch has opened
/// its ticket. The caller passes the path the task actually sits at
/// right now instead, which is what usually makes `SPOOLWAY_TASK_FILE` a
/// path a hook can open — usually, not always: a task with no file of
/// its own at all, such as a `queue add --from -` stream entry, has nothing
/// truthful to hand over, and the caller passes the empty string for that
/// case rather than a name nothing can open. `group_description` is
/// likewise the caller's to resolve — `open_tickets` reads it off whichever
/// task in the group set it,
/// this task's own frontmatter included.
fn open_env(
    repo: &Repo,
    task: &Task,
    group_size: usize,
    group_epic: &str,
    depends_tickets: &str,
    group_description: &str,
    task_file: &str,
) -> BTreeMap<String, String> {
    let front = &task.front;
    BTreeMap::from([
        ("SPOOLWAY_TASK".to_string(), task.id().to_string()),
        ("SPOOLWAY_EVENT".to_string(), "open".to_string()),
        (
            "SPOOLWAY_SOURCE".to_string(),
            front.source.clone().unwrap_or_default(),
        ),
        (
            "SPOOLWAY_GROUP".to_string(),
            front.group.clone().unwrap_or_default(),
        ),
        (
            "SPOOLWAY_GROUP_DESCRIPTION".to_string(),
            group_description.to_string(),
        ),
        (
            "SPOOLWAY_BRANCH".to_string(),
            front.branch.clone().unwrap_or_default(),
        ),
        ("SPOOLWAY_TITLE".to_string(), front.title.clone()),
        ("SPOOLWAY_TASK_FILE".to_string(), task_file.to_string()),
        (
            "SPOOLWAY_PROJECT_KEY".to_string(),
            repo.config.issue_tracking.project_key.clone(),
        ),
        ("SPOOLWAY_GROUP_SIZE".to_string(), group_size.to_string()),
        ("SPOOLWAY_LABELS".to_string(), front.labels.join(",")),
        ("SPOOLWAY_EPIC".to_string(), group_epic.to_string()),
        (
            "SPOOLWAY_DEPENDS_TICKETS".to_string(),
            depends_tickets.to_string(),
        ),
    ])
}

/// Everything a hook script may leave at `SPOOLWAY_OUT` on the `open` event.
/// Every field is optional and order does not matter — a script that wrote
/// none of them reads back as four blank strings rather than an error, the
/// same way a group of one told to skip the epic already does.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub epic: String,
    pub ticket: String,
    /// The short handle for `issue_tracking.key_in_names`; blank when the
    /// hook writes no `slug=` line.
    pub slug: String,
    /// The issue's web address, stored for `terminal-names` to use later;
    /// blank when the hook writes no `url=` line.
    pub url: String,
}

/// Parse the `epic=`, `ticket=`, `slug=` and `url=` lines a hook script
/// leaves at `SPOOLWAY_OUT`, in any order, each optional.
fn read_answer(path: &Path) -> Answer {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut answer = Answer::default();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("epic=") {
            answer.epic = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("ticket=") {
            answer.ticket = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("slug=") {
            answer.slug = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("url=") {
            answer.url = value.trim().to_string();
        }
    }
    answer
}

/// What one synchronous `fetch` hook call came back with — see
/// [`fetch_issue`], the command it backs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchResult {
    /// No hook is configured at all.
    NoHook,
    /// A hook is configured, but its own script names no `fetch` event
    /// anywhere in it — see [`has_fetch_branch`]. Caught before the hook is
    /// ever run: an unmodified script — every install's, until somebody adds
    /// the branch — falls through its own event dispatch to the same
    /// `exit 0` every event outside `open`/`blocked`/`paused` already takes,
    /// which would read as "fetched an issue with nothing on it" rather than
    /// "never implemented" if this only found out by running it.
    NoFetchBranch,
    /// The hook exited zero and wrote something to `SPOOLWAY_OUT`.
    Answered(String),
    /// The hook exited non-zero, or ended without a code at all — the same
    /// [`RunState::Interrupted`] a command step's own run can end in.
    Failed { exit_code: Option<i32> },
}

/// Every variable [`fetch_env`] adds beyond [`COMMON_EVENT_VARS`], plus
/// `SPOOLWAY_OUT`, which [`fetch_issue`] inserts itself once `fetch_env`'s
/// own map comes back — see [`OPEN_EVENT_VARS`] for why this lives beside
/// the function it describes rather than in `commands::hook`.
pub(crate) const FETCH_EVENT_VARS: &[(&str, &str)] = &[
    (
        "SPOOLWAY_REF",
        "the reference exactly as typed — never parsed by spoolway",
    ),
    (
        "SPOOLWAY_OUT",
        "where to write the answer: whatever this prints as JSON",
    ),
];

/// The environment [`fetch_issue`] runs its hook with, minus `SPOOLWAY_OUT`
/// — added by the caller once the tracking directory it points into exists,
/// the same split [`open_env`] and [`open_ticket`] keep.
fn fetch_env(reference: &str, project_key: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SPOOLWAY_EVENT".to_string(), "fetch".to_string()),
        ("SPOOLWAY_REF".to_string(), reference.to_string()),
        ("SPOOLWAY_PROJECT_KEY".to_string(), project_key.to_string()),
    ])
}

/// A filesystem-safe stem for one `fetch` run's tracking files, keyed on the
/// issue reference rather than a task id — `spoolway issue show` runs before
/// any task naming the issue exists, so there is no task to key on.
///
/// [`Runs::key`]'s `<step> · <reference>` cannot be used here: the shipped
/// `github.sh` hook takes a URL, so a URL is the natural thing to type, and a
/// raw `/` in it would put the `.pid` file under a directory that does not
/// exist — the run then fails after the wait while the hook keeps going — and
/// a `../` in it would write those files outside `tracking/` altogether
/// (review finding 18). The reference still reaches the hook verbatim through
/// `SPOOLWAY_REF`; only the stem is reduced, to a slug plus a hash tail so
/// two references that slug alike (`o/r#42` and `o/r/issues/42`) keep their
/// own files.
fn fetch_key(reference: &str) -> String {
    use std::hash::{Hash, Hasher};

    let slug: String = reference
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(60).collect();

    let mut hasher = std::hash::DefaultHasher::new();
    reference.hash(&mut hasher);
    format!("fetch-{slug}-{:x}", hasher.finish())
}

/// Run the `fetch` hook for one issue reference, synchronously, and return
/// what it wrote to `SPOOLWAY_OUT` — `spoolway issue show`'s whole
/// implementation, shaped after [`open_ticket`] for the same reason: there is
/// no later pass to come back and check a detached run on, so this blocks
/// until the hook is actually done rather than answering `Fresh`/`Running`
/// back to a caller with nothing useful to do with either.
///
/// Unlike every other event this one has no task behind it at all — it runs
/// before any task naming this issue even exists — so its tracking files
/// are keyed on the reference through [`fetch_key`], and the environment
/// carries none of a task's own fields: no `SPOOLWAY_TASK`, no
/// `SPOOLWAY_SOURCE`, nothing but the event, the reference and the project
/// key every event gets.
pub fn fetch_issue(repo: &Repo, reference: &str) -> Result<FetchResult> {
    let Some(hook) = hook_path(repo) else {
        return Ok(FetchResult::NoHook);
    };
    // Read before ever spawning anything: a script with no `fetch` branch at
    // all would otherwise exit 0 having written nothing, which is
    // indistinguishable from a real answer of "no such issue" unless this is
    // checked first.
    let script =
        std::fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
    if !has_fetch_branch(&script) {
        return Ok(FetchResult::NoFetchBranch);
    }

    let dir = repo.tracking_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = fetch_key(reference);
    let out_path = dir.join(format!("{key}.out"));
    let _ = std::fs::remove_file(&out_path);

    let mut env = fetch_env(reference, &repo.config.issue_tracking.project_key);
    env.insert("SPOOLWAY_OUT".to_string(), out_path.display().to_string());

    let run_line = crate::platform::quote(&hook.display().to_string());
    let runs = runs(repo);
    runs.start(&key, &run_line, &repo.root, &env)?;

    // Same block as `open_ticket`'s own: `issue show` is a one-shot command
    // too, with no later turn to come back and poll this on.
    let state = loop {
        match runs.state(&key) {
            RunState::Fresh | RunState::Running => {
                std::thread::sleep(Duration::from_millis(25));
            }
            settled => break settled,
        }
    };

    Ok(match state {
        RunState::Exited(0) => {
            FetchResult::Answered(std::fs::read_to_string(&out_path).unwrap_or_default())
        }
        RunState::Exited(code) => FetchResult::Failed {
            exit_code: Some(code),
        },
        RunState::Interrupted => FetchResult::Failed { exit_code: None },
        RunState::Fresh | RunState::Running => unreachable!("the loop above only exits settled"),
    })
}

/// Whether `script`'s own text names the `fetch` event anywhere at all — the
/// static check both [`fetch_issue`] and `spoolway doctor` run rather than
/// ever invoking a hook with `SPOOLWAY_EVENT=fetch` to find out. A plain
/// substring search, not a shell parse: every shipped sample spells its
/// branch `"$SPOOLWAY_EVENT" = fetch` or `-eq 'fetch'`, and a script that
/// mentions the word nowhere has certainly not grown that branch.
pub(crate) fn has_fetch_branch(script: &str) -> bool {
    script.contains("fetch")
}

/// One `# spoolway-requires: <tool> >= <version>` line, parsed out of a
/// hook's own text — see [`required_tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequiredTool {
    pub tool: String,
    pub floor: String,
}

/// Every `# spoolway-requires:` line in `script`, in the order they appear —
/// the static check `spoolway doctor` runs alongside [`has_fetch_branch`] and
/// [`writes_slug_line`], the same plain-text-scan shape. A line that does not
/// parse as `<tool> >= <version>` comes back as its own raw text (`Err`)
/// rather than being dropped or force-fit to the nearest legal shape: the
/// non-goal ruling out a general constraint grammar means `doctor` reports it
/// as unreadable rather than interpreting it, and this is where that text is
/// kept for it to say so with.
pub(crate) fn required_tools(script: &str) -> Vec<Result<RequiredTool, String>> {
    script
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix('#')?.trim();
            let rest = rest.strip_prefix("spoolway-requires:")?.trim();
            Some(match rest.split_once(">=") {
                Some((tool, floor)) if !tool.trim().is_empty() && !floor.trim().is_empty() => {
                    Ok(RequiredTool {
                        tool: tool.trim().to_string(),
                        floor: floor.trim().to_string(),
                    })
                }
                _ => Err(rest.to_string()),
            })
        })
        .collect()
}

/// Whether `script`'s own text ever writes a `slug=` line — the static check
/// `spoolway doctor` runs when `issue_tracking.key_in_names` is on, the same
/// shape as [`has_fetch_branch`]. A plain substring search: every shipped
/// sample writes `echo "slug=..."`, and a script that never mentions the
/// token cannot answer one.
pub(crate) fn writes_slug_line(script: &str) -> bool {
    script.contains("slug=")
}

/// `hook_name`, when [`key_in_names`] is on but the script it names — resolved
/// inside `.spoolway/hooks/` under `checkout`, the same join [`hook_path`]
/// makes — never writes a `slug=` line, so every generated name would carry
/// no prefix. `None` when the flag is off, when `hook_name` is blank or not a
/// bare filename, or when the script cannot be read: each of those is either
/// nothing to report or a different finding already made elsewhere.
///
/// [`key_in_names`] is passed rather than read from a [`Config`] so `doctor`
/// can hand its own freshly loaded copy, the same way every other check here
/// takes `checkout` and `hook_name` directly.
pub(crate) fn missing_slug_line(
    checkout: &Path,
    hook_name: &str,
    key_in_names: bool,
) -> Option<String> {
    if !key_in_names {
        return None;
    }
    let name = hook_name.trim();
    if name.is_empty() || !is_bare_filename(name) {
        return None;
    }
    let script = std::fs::read_to_string(hooks_dir_in(checkout).join(name)).ok()?;
    (!writes_slug_line(&script)).then(|| name.to_string())
}

/// `hook_name`, when the script it names — resolved inside `.spoolway/hooks/`
/// under `checkout`, the same join [`hook_path`] makes — has no `fetch`
/// branch. `None` when `hook_name` is blank, not a bare filename, or cannot
/// be read: each of those is a different finding, already reported by the
/// checks around [`is_bare_filename`].
///
/// Takes `checkout` and `hook_name` rather than a [`Repo`], so `doctor` can
/// pass its own freshly loaded config — the checkout's own copy, not
/// `repo.config`'s — the same way every other finding there does.
pub(crate) fn missing_fetch_branch(checkout: &Path, hook_name: &str) -> Option<String> {
    let name = hook_name.trim();
    if name.is_empty() || !is_bare_filename(name) {
        return None;
    }
    let script = std::fs::read_to_string(hooks_dir_in(checkout).join(name)).ok()?;
    (!has_fetch_branch(&script)).then(|| name.to_string())
}

/// Whether a hook is configured at all — what lets `queue add` skip
/// [`open_ticket`] and its own report entirely rather than call it once per
/// task only to have every call answer [`OpenResult::NoHook`].
pub fn configured(repo: &Repo) -> bool {
    hook_path(repo).is_some()
}

/// The tracker a configured hook opens tickets on, as the queue screen's
/// question names it before any is opened: the hook's own filename with its
/// extension dropped — `github.sh` is `github`, `jira.sh` is `jira`. A hook
/// says nothing else about where its tickets go, and the shipped hooks are
/// named for their trackers, so the name is the one honest thing to say.
pub(crate) fn tracker(repo: &Repo) -> String {
    let hook = repo.config.issue_tracking.hook.trim();
    Path::new(hook)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| hook.to_string())
}

/// How many runs of one hook event may be killed in a row, each without an
/// exit code, before [`fire`] stops starting it again and [`killed`] answers
/// for it instead. The same ceiling a command step gets — see
/// `MAX_COMMAND_KILLS` in [`crate::dispatch`].
pub const MAX_HOOK_KILLS: u32 = 3;

/// Start this task's hook for `event`, unless it already has —see
/// [`RunState::Fresh`]. A no-op with nothing configured, so a project that
/// has never touched `[issue_tracking]` pays for none of this: no directory,
/// no process, no difference in behaviour.
///
/// A run that was killed without an exit code counts as not having started,
/// and is fired again — until [`MAX_HOOK_KILLS`] runs in a row have been
/// killed, after which it is left for [`killed`] to report. Answers
/// `Some(kills)` when this call started such a run again, with how many runs
/// before it were killed, so the caller can say why the hook ran twice.
///
/// `group_open` is how many of this task's own group are still in the open
/// set — [`crate::graph::Graph::group_open`]'s own count — which is both
/// `SPOOLWAY_GROUP_SIZE` and, on the `done` event, what decides
/// `SPOOLWAY_GROUP_LAST`: this task is still counted among the open ones at
/// the moment its own `done` hook fires, so a count of one means nobody else
/// is left.
pub fn fire(repo: &Repo, task: &Task, event: &str, group_open: usize) -> Result<Option<u32>> {
    let Some(hook) = hook_path(repo) else {
        return Ok(None);
    };
    let runs = runs(repo);
    let key = Runs::key(event, task.id());
    // `Interrupted` is a run that died without writing an exit code, which is
    // what a reboot or `wsl --shutdown` mid-hook leaves behind. It never
    // reports a code, so leaving it alone would strand the task on `queued`
    // or `done` for good. `start` clears its stale files first. The count
    // lives beside the run and outlives it, so a hook that something kills
    // every time it starts stops at the ceiling rather than running forever;
    // past it nothing is counted again, however many passes look.
    let refired = match runs.state(&key) {
        RunState::Fresh => None,
        RunState::Interrupted => {
            if runs.kills(&key) >= MAX_HOOK_KILLS {
                return Ok(None);
            }
            let kills = runs.note_kill(&key)?;
            if kills >= MAX_HOOK_KILLS {
                return Ok(None);
            }
            Some(kills)
        }
        _ => return Ok(None),
    };

    let env = build_env(repo, task, event, group_open);
    // Quoted the same way a lane's own environment is — see
    // `platform::quote` — so a checkout path holding a space still
    // reaches the shell as one argument.
    let run_line = crate::platform::quote(&hook.display().to_string());
    runs.start(&key, &run_line, &repo.root, &env)?;
    Ok(refired)
}

/// How many of this task's `event` hook runs were killed in a row without an
/// exit code, once there have been [`MAX_HOOK_KILLS`] of them and [`fire`]
/// has stopped starting it again. `None` otherwise, including while a run
/// that replaced a killed one is still going.
///
/// The dispatcher pauses the task on this the way it does on a non-zero
/// exit, and [`forget`] — what `spoolway resume` calls — clears the count.
pub fn killed(repo: &Repo, task: &Task, event: &str) -> Option<u32> {
    let key = Runs::key(event, task.id());
    let runs = runs(repo);
    let kills = runs.kills(&key);
    (runs.state(&key) == RunState::Interrupted && kills >= MAX_HOOK_KILLS).then_some(kills)
}

/// This task's hook exit code for `event`, once it is known.
///
/// `None` while the hook has not been started, is still running, or ended
/// without a code at all — [`RunState::Interrupted`], which is not a verdict
/// to route on any more than a command step's own is. A caller reacting to a
/// failure sees exactly that: nothing to react to yet.
pub fn exit_code(repo: &Repo, task: &Task, event: &str) -> Option<i32> {
    let key = Runs::key(event, task.id());
    match runs(repo).state(&key) {
        RunState::Exited(code) => Some(code),
        _ => None,
    }
}

/// Forget this task's `event` hook run, so [`fire`]'s own [`RunState::Fresh`]
/// check lets it start over on the next pass — what `spoolway resume` calls
/// on a task a failing hook paused (see [`crate::task::Frontmatter::
/// hook_paused`]), the road out of a hold now that every failing hook pauses
/// its task: a person repairs whatever the hook's own log names, then
/// resumes, rather than a clock retrying an endpoint that may still be down.
pub fn forget(repo: &Repo, task: &Task, event: &str) {
    let key = Runs::key(event, task.id());
    let _ = runs(repo).forget(&key);
}

/// Drop every hook run file this task left under `tracking/`, once it has
/// been archived — the counterpart of [`crate::command_step::Runs`]'
/// own `reclaim_task` for `commands/`, called from the same place in
/// [`crate::dispatch`]'s cleanup.
///
/// A `fetch` run is keyed on an issue reference rather than a task id (see
/// [`fetch_key`]), so nothing here matches one and `spoolway issue show`'s
/// own bookkeeping is left alone.
pub fn reclaim(repo: &Repo, task_id: &str) {
    runs(repo).reclaim_task(task_id);
}

/// [`reclaim`], after first stopping every hook run of this task that is still
/// going — what moving a task back to `pending/` calls.
///
/// A run left going outlives its files: its wrapper writes `<task> · <event>
/// .exit` when the hook ends, so the old code would reappear after the files
/// were removed and [`crate::dispatch::Dispatcher`]'s gate would read it for
/// the task's next run. With no pid file left, [`fire`] would also see the
/// event as [`RunState::Fresh`] and start a second copy beside the first.
pub fn stop_and_reclaim(repo: &Repo, task_id: &str) {
    let runs = runs(repo);
    for key in runs.keys_for_task(task_id) {
        runs.stop(&key);
    }
    runs.reclaim_task(task_id);
}

/// One task hook run whose `.exit` file holds a non-zero code — what
/// `spoolway doctor`, and through it the "before dispatching" popup, names a
/// row for. See [`failures`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFailure {
    pub task: String,
    pub event: String,
    pub exit_code: i32,
    /// The run's own `.log`, which holds the hook's stdout and stderr.
    pub log: PathBuf,
}

/// The events whose failing hook pauses its task, so `spoolway resume` is
/// what clears it: [`forget`] is called for exactly the event
/// `crate::task::Frontmatter::hook_paused` names. The other events —
/// `blocked` and `paused`, which only record a failure, and `open`, which
/// fails `queue add` itself — hold nothing, and their run files stay until
/// [`reclaim`] takes them when the task is archived.
/// `crate::dispatch::Dispatcher::tracking_gate` is the gate these three
/// share.
pub const PAUSING_EVENTS: &[&str] = &[
    crate::pipeline::QUEUED,
    crate::pipeline::STARTED,
    crate::pipeline::DONE,
];

/// Every task hook run under `tracking/` that exited non-zero, in key order.
///
/// A hook run is left exactly as it exited until something removes it, so
/// its `.exit` file staying non-zero is the only evidence of a failure. For
/// one of [`PAUSING_EVENTS`] that is `spoolway resume`, which reads and
/// forgets the run (see [`forget`]); a `blocked`, `paused` or `open` run
/// stays until [`reclaim`] takes it when the task is archived.
///
/// A `<task> · <event>` key whose task is no longer in the queue does not
/// count. [`reclaim`] already deletes those files when a task is archived;
/// this filter is what keeps the list honest when a task was archived by an
/// earlier build that did not, so one failed `open` hook is not named for
/// the life of the project (review finding 64).
///
/// A `fetch` run's key names an issue reference, not a task (see
/// [`fetch_key`]), so it carries no `" · "` and is left out. A failed
/// `spoolway issue show` already printed its own error to whoever ran it,
/// and nothing ever clears a `fetch` run, so naming one would name it for
/// good.
pub fn failures(repo: &Repo) -> Vec<HookFailure> {
    let dir = repo.tracking_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let queued = repo.queued_ids();
    let mut failing: BTreeMap<String, HookFailure> = BTreeMap::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("exit") {
            continue;
        }
        let Some(key) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some((task, event)) = key.split_once(" · ") else {
            continue;
        };
        if !queued.contains(task) {
            continue;
        }
        let code = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| raw.trim().parse::<i32>().ok());
        if let Some(code) = code.filter(|code| *code != 0) {
            failing.insert(
                key.clone(),
                HookFailure {
                    task: task.to_string(),
                    event: event.to_string(),
                    exit_code: code,
                    log: runs(repo).log_path(&key),
                },
            );
        }
    }
    failing.into_values().collect()
}

/// Every variable [`build_env`] adds beyond [`COMMON_EVENT_VARS`], for the
/// four events that fire once a task settles — `queued`, `blocked`, `paused`
/// and `done` — plus `started`, which reuses this same table even though it
/// is not itself a settle: it fires from inside the `queued` arm of
/// `crate::dispatch::Dispatcher::route_reserved_stage`, the moment the task
/// is actually about to leave `queued` rather than merely arrive there. See
/// [`OPEN_EVENT_VARS`] for why this lives beside the function it describes
/// rather than in `commands::hook`.
pub(crate) const DISPATCH_EVENT_VARS: &[(&str, &str)] = &[
    ("SPOOLWAY_TASK", "the task's id"),
    ("SPOOLWAY_FROM", "the step it arrived from"),
    ("SPOOLWAY_SOURCE", "its `source:`"),
    ("SPOOLWAY_GROUP", "its `group:`"),
    ("SPOOLWAY_BRANCH", "its `branch:`"),
    ("SPOOLWAY_TITLE", "its title"),
    ("SPOOLWAY_TASK_FILE", "its path"),
    (
        "SPOOLWAY_GROUP_SIZE",
        "how many tasks in its group are still open",
    ),
    (
        "SPOOLWAY_LABELS",
        "its `labels:`, comma-joined, empty when it has none",
    ),
    ("SPOOLWAY_EPIC", "the epic `open` answered, if any"),
    ("SPOOLWAY_TICKET", "the ticket `open` answered, if any"),
    (
        "SPOOLWAY_GROUP_LAST",
        "`1`, and only on `done`, only on a group's last open task",
    ),
];

fn build_env(repo: &Repo, task: &Task, event: &str, group_open: usize) -> BTreeMap<String, String> {
    let front = &task.front;
    let mut env = BTreeMap::from([
        ("SPOOLWAY_TASK".to_string(), task.id().to_string()),
        ("SPOOLWAY_EVENT".to_string(), event.to_string()),
        (
            "SPOOLWAY_FROM".to_string(),
            front.arrived_from.clone().unwrap_or_default(),
        ),
        (
            "SPOOLWAY_SOURCE".to_string(),
            front.source.clone().unwrap_or_default(),
        ),
        (
            "SPOOLWAY_GROUP".to_string(),
            front.group.clone().unwrap_or_default(),
        ),
        (
            "SPOOLWAY_BRANCH".to_string(),
            front.branch.clone().unwrap_or_default(),
        ),
        ("SPOOLWAY_TITLE".to_string(), front.title.clone()),
        (
            "SPOOLWAY_TASK_FILE".to_string(),
            task.path.display().to_string(),
        ),
        (
            "SPOOLWAY_PROJECT_KEY".to_string(),
            repo.config.issue_tracking.project_key.clone(),
        ),
        ("SPOOLWAY_GROUP_SIZE".to_string(), group_open.to_string()),
        ("SPOOLWAY_LABELS".to_string(), front.labels.join(",")),
        (
            "SPOOLWAY_EPIC".to_string(),
            task.extra_str("epic").to_string(),
        ),
        (
            "SPOOLWAY_TICKET".to_string(),
            task.extra_str("ticket").to_string(),
        ),
    ]);
    // The one variable that is not always set — see the acceptance criterion
    // it comes from: only the `done` event of a group's *last* open task
    // carries it at all, rather than every task carrying it `0` or `1`.
    if event == crate::pipeline::DONE && group_open <= 1 {
        env.insert("SPOOLWAY_GROUP_LAST".to_string(), "1".to_string());
    }
    env
}

// A hook run goes through `command_step::Runs::start`. The fixtures below
// write a hook as a `#!/bin/sh` script made executable by its file mode and
// run as a bare path, relying on the shebang line to say what runs it.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::task::Frontmatter;

    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("tracking-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spoolway/hooks")).unwrap();
        (
            Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config: Config::default(),
                home: root.join(".home"),
            },
            root,
        )
    }

    /// Writes `.spoolway/hooks/<name>`, executable, and points the repo's
    /// `[issue_tracking]` at it.
    fn with_hook(repo: &mut Repo, name: &str, script: &str) {
        let path = repo.checkout.join(".spoolway/hooks").join(name);
        // A script already carrying its own shebang — the real, shipped
        // `github.sh` embedded by `github_open_fixture`/`github_done_fixture`
        // below, `#!/usr/bin/env bash` and all — is written verbatim: adding
        // a second, `#!/bin/sh` one here would not re-shebang it, since only
        // the very first line of a file is ever read that way; it would just
        // sit as a dead comment while `/bin/sh` ran everything below it,
        // including bash-only syntax `/bin/sh` cannot parse.
        let content = if script.starts_with("#!") {
            format!("{}\n", script.trim_end_matches('\n'))
        } else {
            format!("#!/bin/sh\n{script}\n")
        };
        std::fs::write(&path, content).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        repo.config.issue_tracking.hook = name.to_string();
    }

    fn task(id: &str, edit: impl FnOnce(&mut Frontmatter)) -> Task {
        let mut front = Frontmatter {
            id: id.to_string(),
            title: String::new(),
            stage: crate::pipeline::QUEUED.to_string(),
            depends_on: Vec::new(),
            borrowed: false,
            last_report: None,
            blocked_from: None,
            parked_from: None,
            escalated: false,
            parked_by_stop: false,
            resume: None,
            restart: None,
            pipeline: None,
            group: None,
            group_description: None,
            labels: Vec::new(),
            source: None,
            plan: None,
            gate_at: None,
            branch: None,
            base: None,
            run: None,
            starts_from: None,
            base_commit: None,
            patch: None,
            skip: Vec::new(),
            trial: None,
            trial_group: None,
            replay_of: None,
            worktree_path: None,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            attempts: 0,
            paused_at: None,
            paused_by: None,
            hook_paused: None,
            missing_start_branch: None,
            launched_at: None,
            steps: Default::default(),
            rounds: Default::default(),
            arrivals: Default::default(),
            launch_failures: Default::default(),
            launch_busy_since: Default::default(),
            arrived_from: None,
            extra: Default::default(),
        };
        edit(&mut front);
        Task {
            path: PathBuf::from(format!("{id}.md")),
            front,
            body: String::new(),
            arrived_at: Default::default(),
        }
    }

    fn settle(repo: &Repo, task: &Task, event: &str) -> RunState {
        let key = Runs::key(event, task.id());
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(20) {
            match runs(repo).state(&key) {
                RunState::Running | RunState::Fresh => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                done => return done,
            }
        }
        panic!("`{key}` never finished");
    }

    /// A hook run killed without ever writing an exit code — a reboot or
    /// `wsl --shutdown` mid-hook — is not left as the last word on its task.
    /// The next pass fires the hook again, and the second run's own exit
    /// code is what the dispatcher then reads.
    #[test]
    fn a_hook_run_killed_without_an_exit_code_is_fired_again() {
        interrupted_run_is_fired_again(crate::pipeline::STARTED);
    }

    /// The `done` event shares `fire` with `started`, but a stranded `done`
    /// run is the one that keeps a finished task from ever being archived.
    #[test]
    fn a_done_hook_run_killed_without_an_exit_code_is_fired_again() {
        interrupted_run_is_fired_again(crate::pipeline::DONE);
    }

    fn interrupted_run_is_fired_again(event: &str) {
        let (mut repo, _root_guard) = fixture(&format!("interrupted-{event}"));
        let marker = repo.root.join("first-run-seen");
        with_hook(
            &mut repo,
            "once.sh",
            &format!(
                "if [ -e '{m}' ]; then exit 0; fi\ntouch '{m}'\nsleep 60",
                m = marker.display()
            ),
        );
        let t = task("demo", |_| {});
        let key = Runs::key(event, t.id());

        fire(&repo, &t, event, 1).unwrap();
        let pid = runs(&repo).read_pid(&key).expect("the hook run has a pid");
        let started = std::time::Instant::now();
        while !marker.exists() && started.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        crate::headless::kill_group(pid);
        let started = std::time::Instant::now();
        while runs(&repo).state(&key) == RunState::Running
            && started.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(runs(&repo).state(&key), RunState::Interrupted);

        fire(&repo, &t, event, 1).unwrap();
        assert_eq!(settle(&repo, &t, event), RunState::Exited(0));
        assert_eq!(exit_code(&repo, &t, event), Some(0));
    }

    /// Stopping a task's hook runs before reclaiming their files: a hook
    /// still going must not write its exit code back after the files are
    /// gone, and its command must not outlive the move.
    #[test]
    fn stop_and_reclaim_ends_a_running_hook_and_leaves_no_files() {
        let (mut repo, _root_guard) = fixture("stop-and-reclaim");
        with_hook(&mut repo, "slow.sh", "sleep 60");
        let t = task("demo", |_| {});
        let key = Runs::key(crate::pipeline::QUEUED, t.id());

        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        let pid = runs(&repo).read_pid(&key).expect("the hook run has a pid");

        stop_and_reclaim(&repo, t.id());

        assert!(!crate::headless::alive(pid), "the hook outlived the move");
        let left: Vec<_> = std::fs::read_dir(repo.tracking_dir())
            .unwrap()
            .flatten()
            .collect();
        assert!(left.is_empty(), "files left behind: {left:?}");
    }

    /// A hook something kills every time it starts — `systemd-oomd`, a
    /// sandbox — is fired again only until [`MAX_HOOK_KILLS`] runs in a row
    /// have died, then left for [`killed`] to report; [`forget`] clears the
    /// count so `spoolway resume` starts it over.
    #[test]
    fn a_hook_run_killed_every_time_stops_being_fired() {
        let (mut repo, _root_guard) = fixture("killed-every-time");
        let count = repo.root.join("runs");
        with_hook(
            &mut repo,
            "doomed.sh",
            &format!("echo run >>'{c}'\nsleep 60", c = count.display()),
        );
        let t = task("demo", |_| {});
        let event = crate::pipeline::STARTED;
        let key = Runs::key(event, t.id());
        let ran = || {
            std::fs::read_to_string(&count)
                .map(|text| text.lines().count())
                .unwrap_or(0)
        };
        let kill_run = |expected: usize| {
            let started = std::time::Instant::now();
            while ran() < expected && started.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            crate::headless::kill_group(runs(&repo).read_pid(&key).expect("a pid"));
            let started = std::time::Instant::now();
            while runs(&repo).state(&key) == RunState::Running
                && started.elapsed() < std::time::Duration::from_secs(10)
            {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert_eq!(runs(&repo).state(&key), RunState::Interrupted);
        };

        assert_eq!(fire(&repo, &t, event, 1).unwrap(), None);
        kill_run(1);
        assert_eq!(killed(&repo, &t, event), None);
        assert_eq!(fire(&repo, &t, event, 1).unwrap(), Some(1));
        kill_run(2);
        assert_eq!(fire(&repo, &t, event, 1).unwrap(), Some(2));
        kill_run(3);

        for _ in 0..3 {
            assert_eq!(fire(&repo, &t, event, 1).unwrap(), None);
        }
        assert_eq!(ran(), 3, "no fourth run");
        assert_eq!(killed(&repo, &t, event), Some(MAX_HOOK_KILLS));
        assert_eq!(exit_code(&repo, &t, event), None);

        forget(&repo, &t, event);
        assert_eq!(killed(&repo, &t, event), None);
        assert_eq!(fire(&repo, &t, event, 1).unwrap(), None);
        kill_run(4);
        assert_eq!(fire(&repo, &t, event, 1).unwrap(), Some(1));
        crate::headless::kill_group(runs(&repo).read_pid(&key).unwrap());
    }

    /// The whole point of the empty-table default: a project that has never
    /// touched `[issue_tracking]` gets no directory, no process, nothing.
    #[test]
    fn a_blank_hook_fires_nothing() {
        let (repo, _root_guard) = fixture("blank");
        let t = task("demo", |_| {});
        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        assert!(!repo.tracking_dir().join("demo · queued.exit").exists());
        assert_eq!(exit_code(&repo, &t, crate::pipeline::QUEUED), None);
        assert!(failures(&repo).is_empty());
    }

    /// The hook actually runs, with the environment the acceptance criteria
    /// name, and its own exit code is what a later pass reads back.
    #[test]
    fn a_configured_hook_runs_with_the_full_environment() {
        let (mut repo, _root_guard) = fixture("env");
        with_hook(&mut repo, "echo.sh", "env | sort; exit 3");
        // `failures` only names a key whose task is still in the queue — so
        // the task has to actually be there.
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: queued\n---\n",
        )
        .unwrap();
        let t = task("demo", |f| {
            f.title = "cut a thing".into();
            f.group = Some("scanner-rework".into());
            f.source = Some("https://example.com/issues/1".into());
            f.branch = Some("task/demo".into());
            f.labels = vec!["bug".to_string(), "needs-triage".to_string()];
            f.extra
                .insert("epic".into(), serde_norway::Value::String("EPIC-1".into()));
            f.extra
                .insert("ticket".into(), serde_norway::Value::String("TCK-9".into()));
        });

        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::QUEUED),
            RunState::Exited(3)
        );
        assert_eq!(exit_code(&repo, &t, crate::pipeline::QUEUED), Some(3));
        assert_eq!(
            failures(&repo),
            vec![HookFailure {
                task: "demo".into(),
                event: "queued".into(),
                exit_code: 3,
                log: runs(&repo).log_path(&Runs::key("queued", "demo")),
            }]
        );

        let log =
            std::fs::read_to_string(runs(&repo).log_path(&Runs::key("queued", "demo"))).unwrap();
        for expected in [
            "SPOOLWAY_TASK=demo",
            "SPOOLWAY_EVENT=queued",
            "SPOOLWAY_TITLE=cut a thing",
            "SPOOLWAY_GROUP=scanner-rework",
            "SPOOLWAY_SOURCE=https://example.com/issues/1",
            "SPOOLWAY_BRANCH=task/demo",
            "SPOOLWAY_EPIC=EPIC-1",
            "SPOOLWAY_TICKET=TCK-9",
            "SPOOLWAY_GROUP_SIZE=1",
            "SPOOLWAY_LABELS=bug,needs-triage",
        ] {
            assert!(log.contains(expected), "missing `{expected}` in:\n{log}");
        }
        // Only `done` ever carries this, whatever the group's size.
        assert!(!log.contains("SPOOLWAY_GROUP_LAST"));
    }

    /// A failed hook from a task that has since left the queue stops being
    /// named, and [`reclaim`] takes its run files with it at archive time —
    /// so one failure is not reported for the life of the project (review
    /// finding 64).
    #[test]
    fn a_failure_from_an_archived_task_stops_being_named() {
        let (mut repo, _root_guard) = fixture("archived-failure");
        with_hook(&mut repo, "fail.sh", "exit 1");
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: queued\n---\n",
        )
        .unwrap();
        let t = task("demo", |_| {});

        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::QUEUED),
            RunState::Exited(1)
        );
        assert_eq!(
            failures(&repo).len(),
            1,
            "a failing hook of a queued task counts"
        );

        // The task is archived: its file leaves the queue.
        std::fs::remove_file(repo.queue_dir().join("demo.md")).unwrap();
        assert!(
            failures(&repo).is_empty(),
            "a failure from a task no longer in the queue must not count"
        );

        // And `reclaim` clears the files themselves.
        assert!(runs(&repo).log_path(&Runs::key("queued", "demo")).exists());
        reclaim(&repo, "demo");
        assert!(
            !runs(&repo).log_path(&Runs::key("queued", "demo")).exists(),
            "the hook run files were left behind after archiving"
        );
        assert!(
            std::fs::read_dir(repo.tracking_dir())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "nothing of the archived task is left under tracking/"
        );
    }

    /// A `fetch` run's key names an issue reference, not a task, so a failed
    /// `spoolway issue show` is never named: nothing would ever clear it.
    /// A failing task hook beside it still is, so the list is not empty
    /// merely because nothing was read.
    #[test]
    fn a_failed_fetch_is_never_named() {
        let (repo, _root_guard) = fixture("fetch-skipped");
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: paused\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.tracking_dir()).unwrap();
        let key = fetch_key("https://github.com/o/r/issues/42");
        std::fs::write(repo.tracking_dir().join(format!("{key}.exit")), "9\n").unwrap();
        std::fs::write(repo.tracking_dir().join("demo · started.exit"), "1\n").unwrap();
        let named: Vec<String> = failures(&repo).into_iter().map(|f| f.task).collect();
        assert_eq!(named, vec!["demo".to_string()]);
    }

    /// A hook fires once per task per event for the run's whole lifetime — a
    /// second call while the task still sits on the same event must not
    /// start a second process.
    #[test]
    fn a_hook_fires_once_per_task_per_event() {
        let (mut repo, _root_guard) = fixture("once");
        with_hook(&mut repo, "mark.sh", "echo ran");
        let t = task("demo", |_| {});

        fire(&repo, &t, crate::pipeline::BLOCKED, 1).unwrap();
        settle(&repo, &t, crate::pipeline::BLOCKED);
        let key = Runs::key(crate::pipeline::BLOCKED, "demo");
        let first_log = std::fs::read_to_string(runs(&repo).log_path(&key)).unwrap();

        fire(&repo, &t, crate::pipeline::BLOCKED, 1).unwrap();
        // No restart: the log a second run would have rolled aside to
        // `.prev.log` — see `Runs::start` — simply is not there.
        assert!(!runs(&repo).prev_log_path(&key).exists());
        let after = std::fs::read_to_string(runs(&repo).log_path(&key)).unwrap();
        assert_eq!(first_log, after, "a second `fire` must not run it again");
    }

    /// `SPOOLWAY_GROUP_LAST` is the `done` event's own variable, set only when
    /// this task is the last of its group still open — and absent on every
    /// other event and every task that is not last.
    #[test]
    fn group_last_is_set_only_for_the_last_open_task_at_done() {
        let (repo, _root_guard) = fixture("group-last");
        let t = task("demo", |f| f.group = Some("g".into()));

        assert!(build_env(&repo, &t, crate::pipeline::DONE, 1).contains_key("SPOOLWAY_GROUP_LAST"));
        assert!(
            !build_env(&repo, &t, crate::pipeline::DONE, 2).contains_key("SPOOLWAY_GROUP_LAST"),
            "two still open — this one is not last"
        );
        assert!(
            !build_env(&repo, &t, crate::pipeline::BLOCKED, 1).contains_key("SPOOLWAY_GROUP_LAST"),
            "only the done event ever carries it"
        );
    }

    /// `spoolway hook contract` prints [`COMMON_EVENT_VARS`],
    /// [`OPEN_EVENT_VARS`], [`DISPATCH_EVENT_VARS`] and [`FETCH_EVENT_VARS`]
    /// rather than a second, hand-kept copy of what these functions actually
    /// build — this is what keeps the two from drifting apart: the name set
    /// each table documents has to be exactly the name set a real call
    /// produces, common variables included.
    #[test]
    fn open_dispatch_and_fetch_vars_match_a_real_environment() {
        use std::collections::BTreeSet;

        fn names(table: &[(&'static str, &'static str)]) -> BTreeSet<&'static str> {
            table.iter().map(|(name, _)| *name).collect()
        }

        let (repo, _root_guard) = fixture("hook-contract-vars");
        let t = task("demo", |f| f.group = Some("g".into()));

        // `open_env` never sees `SPOOLWAY_OUT` — `open_ticket` adds it once
        // the tracking directory it points into exists — so it is added
        // here exactly the way that caller does, rather than expected of
        // `open_env` itself.
        let mut open = open_env(&repo, &t, 1, "", "", "", "demo.md");
        open.insert("SPOOLWAY_OUT".to_string(), String::new());
        let open_keys: BTreeSet<&str> = open.keys().map(String::as_str).collect();
        assert_eq!(
            open_keys,
            names(COMMON_EVENT_VARS)
                .into_iter()
                .chain(names(OPEN_EVENT_VARS))
                .collect(),
        );

        // `SPOOLWAY_GROUP_LAST` is the one variable `build_env` only
        // sometimes carries — called here the way it is at a group's last
        // open task on `done`, so this run's own keys are the full set
        // `DISPATCH_EVENT_VARS` documents.
        let dispatch = build_env(&repo, &t, crate::pipeline::DONE, 1);
        let dispatch_keys: BTreeSet<&str> = dispatch.keys().map(String::as_str).collect();
        assert_eq!(
            dispatch_keys,
            names(COMMON_EVENT_VARS)
                .into_iter()
                .chain(names(DISPATCH_EVENT_VARS))
                .collect(),
        );

        // `fetch_env` never sees `SPOOLWAY_OUT` either — `fetch_issue` adds
        // it the same way `open_ticket` adds its own.
        let mut fetch = fetch_env("o/r#42", "");
        fetch.insert("SPOOLWAY_OUT".to_string(), String::new());
        let fetch_keys: BTreeSet<&str> = fetch.keys().map(String::as_str).collect();
        assert_eq!(
            fetch_keys,
            names(COMMON_EVENT_VARS)
                .into_iter()
                .chain(names(FETCH_EVENT_VARS))
                .collect(),
        );
    }

    /// One `# spoolway-requires:` line per tool a hook declares, in the order
    /// they appear, alongside a line that does not parse as `<tool> >=
    /// <version>` — reported back as its own raw text rather than dropped or
    /// force-fit, since the non-goal rules out a general constraint grammar.
    #[test]
    fn required_tools_reads_every_declaration_line() {
        let script = "#!/bin/sh\n\
             # spoolway-requires: gh >= 2.97.0\n\
             #\n\
             # spoolway-requires: jq >= 1.6\n\
             # spoolway-requires: nonsense-line\n\
             echo hi\n";
        let found = required_tools(script);
        assert_eq!(
            found,
            vec![
                Ok(RequiredTool {
                    tool: "gh".to_string(),
                    floor: "2.97.0".to_string(),
                }),
                Ok(RequiredTool {
                    tool: "jq".to_string(),
                    floor: "1.6".to_string(),
                }),
                Err("nonsense-line".to_string()),
            ]
        );
    }

    /// A script with no `# spoolway-requires:` line at all reads back empty
    /// — the acceptance criterion that a hook declaring nothing is checked
    /// exactly as it is today.
    #[test]
    fn required_tools_is_empty_with_no_declaration() {
        assert!(required_tools("#!/bin/sh\necho hi\n").is_empty());
    }

    /// The whole of what makes `hook_path`'s join safe: a name holding a
    /// separator, or naming `.` or `..`, is never one path component
    /// (review finding 56).
    #[test]
    fn is_bare_filename_refuses_anything_that_is_not_one_component() {
        assert!(is_bare_filename("github.sh"));
        assert!(is_bare_filename("jira-cloud.sh"));
        for escaping in ["../evil.sh", "sub/dir.sh", "a\\b.sh", ".", ".."] {
            assert!(!is_bare_filename(escaping), "`{escaping}` is not bare");
        }
    }

    /// A reference that carries `/` or `../` — a URL is the natural thing to
    /// type at `spoolway issue show` — is reduced to one path component before
    /// it names any tracking file (review finding 18).
    #[test]
    fn fetch_key_is_always_one_safe_component() {
        for reference in [
            "https://github.com/o/r/issues/42",
            "../../etc/passwd",
            "o/r#42",
            "PROJ-123",
        ] {
            let key = fetch_key(reference);
            assert!(is_bare_filename(&key), "`{reference}` → `{key}`");
            assert!(key.starts_with("fetch-"), "`{reference}` → `{key}`");
        }
        // Two references that slug alike still get their own files.
        assert_ne!(fetch_key("o/r#42"), fetch_key("o/r/issues/42"));
    }

    /// A `hook` that is not a bare filename runs nothing at all — the same
    /// as a blank one — rather than being joined onto the hooks directory
    /// and trusted to stay inside it.
    #[test]
    fn a_hook_naming_a_path_fires_nothing() {
        let (mut repo, _root_guard) = fixture("path-traversal");
        repo.config.issue_tracking.hook = "../escaped".into();
        let t = task("demo", |_| {});

        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        assert_eq!(exit_code(&repo, &t, crate::pipeline::QUEUED), None);
        assert!(
            std::fs::read_dir(repo.tracking_dir())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "nothing was ever started"
        );
    }

    /// [`forget`] is the whole road out of a hook pause: a failed run stays
    /// failed until this clears it, and the next `fire` sees `Fresh` again
    /// and actually restarts it.
    #[test]
    fn forget_lets_fire_restart_a_failed_run() {
        let (mut repo, _root_guard) = fixture("forget");
        with_hook(&mut repo, "flaky.sh", "exit 1");
        let t = task("demo", |_| {});

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        settle(&repo, &t, crate::pipeline::DONE);
        assert_eq!(exit_code(&repo, &t, crate::pipeline::DONE), Some(1));

        forget(&repo, &t, crate::pipeline::DONE);
        assert_eq!(
            exit_code(&repo, &t, crate::pipeline::DONE),
            None,
            "forgetting the run must clear its exit code"
        );
        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );
    }

    /// No hook configured means `open_ticket` starts no process at all —
    /// the same "no configuration, no behaviour" contract [`fire`] gives.
    #[test]
    fn open_ticket_with_no_hook_starts_nothing() {
        let (repo, _root_guard) = fixture("open-no-hook");
        let t = task("demo", |_| {});
        assert_eq!(
            open_ticket(&repo, &t, 1, "", "", "", "demo.md").unwrap(),
            OpenResult::NoHook
        );
    }

    /// A hook that answers cleanly hands back exactly what it wrote to
    /// `SPOOLWAY_OUT`. The hook builds the whole issue body itself, from
    /// `SPOOLWAY_TASK_FILE` and `SPOOLWAY_GROUP_DESCRIPTION` alone — this
    /// proves both actually reach it, rather than a file spoolway rendered
    /// for it.
    #[test]
    fn open_ticket_reads_the_answer_and_hands_the_hook_the_task_file_and_description() {
        let (mut repo, _root_guard) = fixture("open-answers");
        with_hook(
            &mut repo,
            "open.sh",
            r#"cat "$SPOOLWAY_TASK_FILE" >"$SPOOLWAY_OUT.task-file.seen"
               echo "$SPOOLWAY_GROUP_DESCRIPTION" >"$SPOOLWAY_OUT.description.seen"
               { echo "epic=$SPOOLWAY_GROUP_SIZE-parents:$SPOOLWAY_DEPENDS_TICKETS"; \
                 echo "ticket=acme/app#43"; } >"$SPOOLWAY_OUT""#,
        );
        let t = task("scan-pending", |f| {
            f.group = Some("scanner-rework".into());
            f.source = Some("/plans/scanner-rework.html".into());
        });

        // The task this task came from, sitting wherever `queue add`
        // reads it from mid-flight — not `t.path`, which names where it
        // will land in the queue, a file that does not exist yet.
        let source_doc = repo.root.join("scan-pending.md");
        std::fs::write(&source_doc, "the task's own live contents\n").unwrap();

        let result = open_ticket(
            &repo,
            &t,
            3,
            "",
            "acme/app#42",
            "Mirrors a group of scans.",
            &source_doc.display().to_string(),
        )
        .unwrap();
        assert_eq!(
            result,
            OpenResult::Answered {
                epic: "3-parents:acme/app#42".into(),
                ticket: "acme/app#43".into(),
                slug: String::new(),
                url: String::new(),
            }
        );

        let key = Runs::key("open", "scan-pending");
        // `SPOOLWAY_TASK_FILE` names a path the hook can actually open and
        // read — the task's real, current contents, not the queue path
        // `validate_batch` has merely decided on.
        let task_file_seen = std::fs::read_to_string(
            repo.tracking_dir()
                .join(format!("{key}.out.task-file.seen")),
        )
        .unwrap();
        assert_eq!(task_file_seen, "the task's own live contents\n");

        let description_seen = std::fs::read_to_string(
            repo.tracking_dir()
                .join(format!("{key}.out.description.seen")),
        )
        .unwrap();
        assert_eq!(description_seen.trim(), "Mirrors a group of scans.");
    }

    /// A hook that exits non-zero is `Failed`, carrying the code back rather
    /// than an answer — nothing in `SPOOLWAY_OUT` is trusted once the script
    /// itself said it did not finish cleanly.
    #[test]
    fn open_ticket_reports_a_failed_exit() {
        let (mut repo, _root_guard) = fixture("open-fails");
        with_hook(&mut repo, "boom.sh", "exit 7");
        let t = task("split-fields", |_| {});

        assert_eq!(
            open_ticket(&repo, &t, 1, "", "", "", "split-fields.md").unwrap(),
            OpenResult::Failed { exit_code: Some(7) }
        );
    }

    /// No hook configured means `fetch_issue` starts no process at all — the
    /// same "no configuration, no behaviour" contract every other event
    /// gives.
    #[test]
    fn fetch_issue_with_no_hook_starts_nothing() {
        let (repo, _root_guard) = fixture("fetch-no-hook");
        assert_eq!(fetch_issue(&repo, "57").unwrap(), FetchResult::NoHook);
    }

    /// A hook script that has never heard of `fetch` — every install's,
    /// before somebody adds the branch — is caught before it is ever run,
    /// not left to answer an empty issue that reads as a real one.
    #[test]
    fn fetch_issue_with_no_fetch_branch_runs_nothing() {
        let (mut repo, _root_guard) = fixture("fetch-unimplemented");
        with_hook(
            &mut repo,
            "old.sh",
            "case \"$SPOOLWAY_EVENT\" in blocked|paused) ;; *) exit 0 ;; esac",
        );
        assert_eq!(
            fetch_issue(&repo, "57").unwrap(),
            FetchResult::NoFetchBranch
        );
        assert!(
            std::fs::read_dir(repo.tracking_dir())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "nothing was ever started"
        );
    }

    /// A hook that answers cleanly hands back exactly what it wrote to
    /// `SPOOLWAY_OUT`, with the reference and project key it was given.
    #[test]
    fn fetch_issue_reads_the_answer_back() {
        let (mut repo, _root_guard) = fixture("fetch-answers");
        repo.config.issue_tracking.project_key = "acme/app".into();
        with_hook(
            &mut repo,
            "fetch.sh",
            r#"if [ "$SPOOLWAY_EVENT" = fetch ]; then
                 printf '{"ref":"%s","project":"%s"}' "$SPOOLWAY_REF" "$SPOOLWAY_PROJECT_KEY" \
                   > "$SPOOLWAY_OUT"
                 exit 0
               fi"#,
        );

        let result = fetch_issue(&repo, "57").unwrap();
        assert_eq!(
            result,
            FetchResult::Answered(r#"{"ref":"57","project":"acme/app"}"#.to_string())
        );
    }

    /// A hook that exits non-zero on `fetch` is `Failed`, the same as `open`.
    #[test]
    fn fetch_issue_reports_a_failed_exit() {
        let (mut repo, _root_guard) = fixture("fetch-fails");
        with_hook(
            &mut repo,
            "boom.sh",
            "case \"$SPOOLWAY_EVENT\" in fetch) exit 9 ;; esac",
        );

        assert_eq!(
            fetch_issue(&repo, "57").unwrap(),
            FetchResult::Failed { exit_code: Some(9) }
        );
    }

    /// The static check both `fetch_issue` and `doctor` run before ever
    /// invoking a hook — a plain substring search over the script's own text.
    #[test]
    fn has_fetch_branch_is_a_plain_substring_search() {
        assert!(has_fetch_branch(
            "case \"$SPOOLWAY_EVENT\" in fetch) ... ;; esac"
        ));
        assert!(!has_fetch_branch(
            "case \"$SPOOLWAY_EVENT\" in open) ... ;; esac"
        ));
    }

    /// What `doctor` reports: a configured hook whose own script has no
    /// `fetch` branch names the script, and one that does — or no hook at
    /// all — reports nothing.
    #[test]
    fn missing_fetch_branch_names_a_hook_with_no_fetch_case() {
        let (repo, _root_guard) = fixture("missing-fetch");
        std::fs::write(
            repo.checkout.join(".spoolway/hooks/old.sh"),
            "#!/bin/sh\ncase \"$SPOOLWAY_EVENT\" in blocked|paused) ;; *) exit 0 ;; esac\n",
        )
        .unwrap();
        std::fs::write(
            repo.checkout.join(".spoolway/hooks/current.sh"),
            "#!/bin/sh\ncase \"$SPOOLWAY_EVENT\" in fetch) ;; esac\n",
        )
        .unwrap();

        assert_eq!(
            missing_fetch_branch(&repo.checkout, "old.sh"),
            Some("old.sh".to_string())
        );
        assert_eq!(missing_fetch_branch(&repo.checkout, "current.sh"), None);
        assert_eq!(missing_fetch_branch(&repo.checkout, ""), None);
        assert_eq!(missing_fetch_branch(&repo.checkout, "../escaped"), None);
    }

    /// `read_answer` picks up the two new optional lines, in any order, and a
    /// hook writing neither reads them back blank — exactly as a missing
    /// `epic=` already does.
    #[test]
    fn read_answer_parses_slug_and_url_when_present_and_blank_when_not() {
        let (repo, _root_guard) = fixture("read-answer-slug");
        let with = repo.tracking_dir().join("with.out");
        std::fs::create_dir_all(repo.tracking_dir()).unwrap();
        std::fs::write(
            &with,
            "url=https://acme.atlassian.net/browse/PROJ-12\nticket=PROJ-13\nslug=proj-12\n",
        )
        .unwrap();
        assert_eq!(
            read_answer(&with),
            Answer {
                epic: String::new(),
                ticket: "PROJ-13".into(),
                slug: "proj-12".into(),
                url: "https://acme.atlassian.net/browse/PROJ-12".into(),
            }
        );

        let without = repo.tracking_dir().join("without.out");
        std::fs::write(&without, "epic=PROJ-12\nticket=PROJ-13\n").unwrap();
        let answer = read_answer(&without);
        assert!(answer.slug.is_empty());
        assert!(answer.url.is_empty());
    }

    /// What `doctor` reports when `key_in_names` is on: a configured hook
    /// whose own script never writes `slug=` names the script, and one that
    /// does — or the flag being off — reports nothing.
    #[test]
    fn missing_slug_line_names_a_hook_that_never_writes_one() {
        let (repo, _root_guard) = fixture("missing-slug");
        std::fs::write(
            repo.checkout.join(".spoolway/hooks/old.sh"),
            "#!/bin/sh\n{ echo \"epic=$SPOOLWAY_EPIC\"; echo \"ticket=x\"; } >\"$SPOOLWAY_OUT\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.checkout.join(".spoolway/hooks/current.sh"),
            "#!/bin/sh\necho \"slug=proj-12\" >>\"$SPOOLWAY_OUT\"\n",
        )
        .unwrap();

        assert_eq!(
            missing_slug_line(&repo.checkout, "old.sh", true),
            Some("old.sh".to_string())
        );
        assert_eq!(missing_slug_line(&repo.checkout, "current.sh", true), None);
        // The flag being off is nothing to report, whatever the script says.
        assert_eq!(missing_slug_line(&repo.checkout, "old.sh", false), None);
        assert_eq!(missing_slug_line(&repo.checkout, "", true), None);
        assert_eq!(missing_slug_line(&repo.checkout, "../escaped", true), None);
    }

    // The shipped `github.sh`'s `done` branch, run for real rather than
    // asserted on by substring: a stub `gh` on `PATH` ahead of the real one
    // (the way `spoolway stack`'s own end-to-end suite stubs `gh` through
    // `SPOOLWAY_GH` — `github.sh` has no such override, so the redirection
    // has to happen at `PATH` instead) records every call it receives under
    // `stub/` inside the fixture's own root, which is also the hook's `$PWD`
    // — `Runs::start` always runs a hook with `cwd` set to `repo.root` — so
    // every test fixture's `stub/` directory is already isolated by
    // `fixture`'s own scratch root and nothing here needs a process-global
    // environment variable.

    /// A `gh` stand-in for the `done`-branch tests below, and the `open`-
    /// branch label tests beside them: it logs every invocation, answers
    /// `pr view`'s `--json url` shape from a file the test writes first,
    /// captures whatever `--body` argument `gh issue comment` is given
    /// (`gh pr comment` is still handled, for a test that wants to prove the
    /// hook never calls it), answers `gh label list` from
    /// `stub/existing_labels` (one name per line, blank when the file is
    /// absent), mints an increasing `https://github.com/o/r/issues/<n>` for
    /// every `gh issue create`, and can be told to fail any of its calls
    /// independently — `stub/pr_view_fail` for the branch lookup,
    /// `stub/issue_edit_fail` for the label swap, `stub/issue_comment_fail`
    /// for the final "Ready for review" comment, `stub/label_list_fail` and
    /// `stub/label_create_fail` for the label tests — which is what the
    /// failure-propagation tests below each need one of.
    fn write_stub_gh(bin_dir: &std::path::Path) {
        std::fs::create_dir_all(bin_dir).unwrap();
        let script = r#"#!/bin/sh
echo "$*" >> stub/gh.log
# `--body` always takes its text as the very next argument here — captured
# by walking "$@" rather than assuming a fixed position, since `pr comment`
# and `issue comment` each carry a different number of flags ahead of it.
body=""
prev=""
for arg in "$@"; do
  [ "$prev" = "--body" ] && body=$arg
  prev=$arg
done
case "$1 $2" in
  "pr view")
    [ -f stub/pr_view_fail ] && exit 1
    cat stub/pr_url
    ;;
  "pr comment")
    [ -f stub/pr_comment_fail ] && exit 1
    printf '%s' "$body" > stub/pr_comment.received
    echo posted >> stub/pr_comment.log
    ;;
  "issue edit")
    [ -f stub/issue_edit_fail ] && exit 1
    echo "$*" >> stub/issue_edit.log
    ;;
  "issue comment")
    [ -f stub/issue_comment_fail ] && exit 1
    printf '%s' "$body" > stub/issue_comment.received
    echo posted >> stub/issue_comment.log
    ;;
  "issue close")
    echo "$*" >> stub/close.log
    ;;
  "label list")
    [ -f stub/label_list_fail ] && exit 1
    cat stub/existing_labels 2>/dev/null
    ;;
  "label create")
    [ -f stub/label_create_fail ] && exit 1
    echo "$*" >> stub/label_create.log
    ;;
  "issue create")
    [ -f stub/issue_create_fail ] && exit 1
    echo "$*" >> stub/issue_create.log
    n=$(cat stub/issue_counter 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > stub/issue_counter
    echo "https://github.com/o/r/issues/$n"
    ;;
esac
exit 0
"#;
        let path = bin_dir.join("gh");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    /// `real` (a shipped hook's own text, `#!/usr/bin/env bash` and all)
    /// with a `PATH=` override inserted right after its own shebang line,
    /// so the stub `gh` in `bin_dir` resolves ahead of any real one, and the
    /// shebang itself stays the file's first line — moving it would leave
    /// nothing to make `with_hook` write the script out verbatim instead of
    /// wrapping it in a `#!/bin/sh` of its own.
    fn pin_path_after_shebang(real: &str, bin_dir: &std::path::Path) -> String {
        let (shebang, rest) = real.split_once('\n').expect("a shipped hook has a body");
        format!("{shebang}\nPATH=\"{}:$PATH\"\n{rest}", bin_dir.display())
    }

    /// A fixture whose `.spoolway/hooks/github.sh` is the real shipped
    /// script — [`crate::assets::HOOK_SCRIPTS`], not a paraphrase of it —
    /// with one line prepended to put the stub `gh` above ahead of the
    /// (absent) real one on `PATH`. Returns the fixture, a `demo` task on
    /// `task/demo` naming `ticket` as given, and the `stub/` directory the
    /// test still needs to seed before calling [`fire`].
    fn github_done_fixture(
        name: &str,
        ticket: &str,
    ) -> (Repo, Task, PathBuf, crate::scratch::ScratchRoot) {
        let (mut repo, root_guard) = fixture(name);
        let stub = repo.root.join("stub");
        write_stub_gh(&stub.join("bin"));

        let real = crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| *known == "github.sh")
            .expect("github.sh is a shipped hook")
            .1;
        let script = pin_path_after_shebang(real, &stub.join("bin"));
        with_hook(&mut repo, "github.sh", &script);

        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: queued\n---\n",
        )
        .unwrap();
        let t = task("demo", |f| {
            f.branch = Some("task/demo".into());
            f.extra
                .insert("ticket".into(), serde_norway::Value::String(ticket.into()));
        });
        (repo, t, stub, root_guard)
    }

    /// [`github_done_fixture`]'s own counterpart for the `open` branch's
    /// label tests below: the same real, shipped `github.sh` against the
    /// same stub `gh`, but with nothing pinned to `ticket:` — `open_ticket`
    /// is what creates one — and `stub/existing_labels` seeded with the two
    /// `spoolway:*` labels every group and task already carries, so a test
    /// naming its own labels beside them proves the hook creates only what
    /// is actually missing.
    fn github_open_fixture(name: &str) -> (Repo, PathBuf, crate::scratch::ScratchRoot) {
        let (mut repo, root_guard) = fixture(name);
        let stub = repo.root.join("stub");
        write_stub_gh(&stub.join("bin"));
        std::fs::write(
            stub.join("existing_labels"),
            "spoolway:group\nspoolway:task\n",
        )
        .unwrap();

        let real = crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| *known == "github.sh")
            .expect("github.sh is a shipped hook")
            .1;
        let script = pin_path_after_shebang(real, &stub.join("bin"));
        with_hook(&mut repo, "github.sh", &script);
        (repo, stub, root_guard)
    }

    /// Acceptance criteria: `open` creates only the labels `gh label list`
    /// does not already show, never with `--force`, and adds every label
    /// to both the task's own ticket and its group's epic, beside the
    /// `spoolway:*` labels each already carries.
    #[test]
    fn github_sh_open_creates_missing_labels_and_adds_them_to_both_issues() {
        let (repo, stub, _root_guard) = github_open_fixture("open-labels-new-epic");
        let t = task("demo", |f| {
            f.title = "add labels".into();
            f.group = Some("labelled-group".into());
            f.labels = vec!["bug".to_string(), "needs-triage".to_string()];
        });

        let result = open_ticket(&repo, &t, 1, "", "", "a group needing labels", "").unwrap();
        let OpenResult::Answered { epic, ticket, .. } = result else {
            panic!("expected an answered open: {result:?}");
        };
        assert_eq!(epic, "https://github.com/o/r/issues/1");
        assert_eq!(ticket, "https://github.com/o/r/issues/2");

        let create_log = std::fs::read_to_string(stub.join("label_create.log")).unwrap();
        assert!(create_log.contains("bug"), "{create_log}");
        assert!(create_log.contains("needs-triage"), "{create_log}");
        assert!(
            !create_log.contains("spoolway:"),
            "an already-existing label was created again: {create_log}"
        );

        let gh_log = std::fs::read_to_string(stub.join("gh.log")).unwrap();
        assert!(
            !gh_log.contains("--force"),
            "an existing label must never be forced: {gh_log}"
        );

        let issue_log = std::fs::read_to_string(stub.join("issue_create.log")).unwrap();
        let mut calls = issue_log.lines();
        let epic_call = calls.next().unwrap();
        let ticket_call = calls.next().unwrap();
        for call in [epic_call, ticket_call] {
            assert!(call.contains("--label bug"), "{call}");
            assert!(call.contains("--label needs-triage"), "{call}");
        }
        assert!(epic_call.contains("--label spoolway:group"), "{epic_call}");
        assert!(
            ticket_call.contains("--label spoolway:task"),
            "{ticket_call}"
        );
    }

    /// Review finding, ported: GitHub treats a label's name case-
    /// insensitively, so a task naming `Bug` where the repository already
    /// has `bug` must not try to create a second one — that call would fail
    /// as a duplicate and take the whole `open` event down with it, exactly
    /// the failure a case-sensitive match let through. This also proves
    /// `gh label list` is asked for more than its own default 30-row limit
    /// — a repository with more labels than that would otherwise read an
    /// older one as missing the same way.
    #[test]
    fn github_sh_open_matches_an_existing_label_case_insensitively() {
        let (repo, stub, _root_guard) = github_open_fixture("open-labels-case-insensitive");
        std::fs::write(
            stub.join("existing_labels"),
            "spoolway:group\nspoolway:task\nBug\n",
        )
        .unwrap();
        let t = task("demo", |f| {
            f.title = "case-insensitive label".into();
            f.group = Some("labelled-group".into());
            f.labels = vec!["bug".to_string()];
        });

        let result = open_ticket(&repo, &t, 1, "", "", "a group needing labels", "").unwrap();
        assert!(matches!(result, OpenResult::Answered { .. }), "{result:?}");

        assert!(
            !stub.join("label_create.log").exists(),
            "`bug` was created again though `Bug` already existed"
        );

        let gh_log = std::fs::read_to_string(stub.join("gh.log")).unwrap();
        assert!(
            gh_log.contains("label list") && gh_log.contains("-L 1000"),
            "gh label list must ask past its own 30-row default: {gh_log}"
        );
    }

    /// A group's epic already open from an earlier task in the same batch
    /// still picks up a later task's own labels — added by `gh issue edit
    /// --add-label`, since `open` never creates the epic a second time —
    /// the union effect that gives the group issue every task's labels
    /// rather than only the first one's.
    #[test]
    fn github_sh_open_adds_a_later_tasks_labels_onto_an_already_open_epic() {
        let (repo, stub, _root_guard) = github_open_fixture("open-labels-existing-epic");
        let t = task("demo", |f| {
            f.title = "second of the group".into();
            f.group = Some("labelled-group".into());
            f.labels = vec!["docs".to_string()];
        });

        let result = open_ticket(
            &repo,
            &t,
            2,
            "https://github.com/o/r/issues/9",
            "",
            "a group needing labels",
            "",
        )
        .unwrap();
        let OpenResult::Answered { epic, .. } = result else {
            panic!("expected an answered open: {result:?}");
        };
        assert_eq!(
            epic, "https://github.com/o/r/issues/9",
            "the epic was not recreated"
        );

        let edit_log = std::fs::read_to_string(stub.join("issue_edit.log")).unwrap();
        assert!(
            edit_log.contains("https://github.com/o/r/issues/9"),
            "{edit_log}"
        );
        assert!(edit_log.contains("--add-label docs"), "{edit_log}");

        // Only the ticket goes through `issue create` here — the epic
        // already existed, so it must not appear a second time.
        let issue_log = std::fs::read_to_string(stub.join("issue_create.log")).unwrap();
        assert_eq!(issue_log.lines().count(), 1, "{issue_log}");
    }

    /// Acceptance criterion: `done` leaves the ticket open, never comments on
    /// the pull request itself, relabels the ticket for review, comments
    /// "Ready for review in &lt;PR URL&gt;" on it — the line a merge sweep
    /// reads to tell this ticket's own pull request apart from a sibling's —
    /// and closes nothing itself.
    #[test]
    fn github_sh_done_hands_the_ticket_to_its_pull_request_without_closing_it() {
        let (repo, t, stub, _root_guard) =
            github_done_fixture("done-handoff", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(0)
        );

        assert!(
            !stub.join("pr_comment.log").exists(),
            "github.sh commented on the pull request again"
        );
        let edit = std::fs::read_to_string(stub.join("issue_edit.log")).unwrap();
        assert!(
            edit.contains("--remove-label spoolway:in-progress"),
            "{edit}"
        );
        assert!(edit.contains("--add-label spoolway:review"), "{edit}");
        let ready = std::fs::read_to_string(stub.join("issue_comment.received")).unwrap();
        assert_eq!(
            ready, "Ready for review in https://github.com/o/r/pull/9",
            "{ready}"
        );
        assert!(
            !stub.join("close.log").exists(),
            "the ticket or epic was closed"
        );
    }

    /// Review finding, ported: a failed pull-request lookup used to read
    /// exactly like "no pull request yet" and the hook would exit clean,
    /// though by the time `done` fires `spoolway stack` has always already
    /// opened one — so this is a real failure, not a normal case to skip
    /// past.
    #[test]
    fn github_sh_done_fails_when_the_pull_request_lookup_fails() {
        let (repo, t, stub, _root_guard) =
            github_done_fixture("done-lookup-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_view_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(!stub.join("pr_comment.log").exists());
        assert!(!stub.join("issue_edit.log").exists());
        assert!(!stub.join("issue_comment.log").exists());

        // Review finding: `pr=$(gh pr view ...)` fails inside its own
        // command substitution, and this trap is inherited into that
        // subshell — without the `BASH_SUBSHELL` guard in `hook_trace` the
        // trace would print twice, once from the subshell and once more
        // from the assignment once its own non-zero exit reaches the outer
        // shell. Exactly one trace, naming the whole assignment line.
        let log = std::fs::read_to_string(
            runs(&repo).log_path(&Runs::key(crate::pipeline::DONE, "demo")),
        )
        .unwrap();
        assert_eq!(
            log.matches("command failed (exit 1)").count(),
            1,
            "the trace must print exactly once: {log}"
        );
        assert!(log.contains("pr=$(gh pr view"), "{log}");
    }

    /// The same "never silently skip" rule applies when the lookup succeeds
    /// but finds nothing: a `done` this hook was fired for always has a
    /// pull request behind it, so no result is as much a failure as a
    /// nonzero exit is.
    #[test]
    fn github_sh_done_fails_when_no_pull_request_is_found() {
        let (repo, t, stub, _root_guard) =
            github_done_fixture("done-no-pr", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(!stub.join("pr_comment.log").exists());
    }

    /// A failed label swap must stop before the final "Ready for review"
    /// comment ever posts — nothing downstream of the failed call runs.
    #[test]
    fn github_sh_done_stops_when_the_label_swap_fails() {
        let (repo, t, stub, _root_guard) =
            github_done_fixture("done-label-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();
        std::fs::write(stub.join("issue_edit_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(!stub.join("issue_comment.log").exists());
    }

    /// Review finding, ported: a failed final comment used to be swallowed
    /// by the done branch's own unconditional success, so the hook could
    /// exit clean without ever describing the handoff as the acceptance
    /// criteria require. The label swap has already landed by this point, so
    /// a retry only needs to redo the one comment that failed.
    #[test]
    fn github_sh_done_fails_when_the_final_comment_fails() {
        let (repo, t, stub, _root_guard) =
            github_done_fixture("done-comment-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();
        std::fs::write(stub.join("issue_comment_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(stub.join("issue_edit.log").exists());
    }
}
