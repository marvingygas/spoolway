//! The `[issue_tracking]` hook: one script a project names, run once per task
//! on each of the four events a task can come to rest on — `queued`,
//! `blocked`, `paused` and `done` — plus two more that are not task events at
//! all: `open`, which [`open_ticket`] runs synchronously from `queue add`
//! itself, before any of those four could ever fire, and `fetch`, which
//! [`fetch_issue`] runs synchronously from `spoolway issue show`, before any
//! task tied to the issue it names even exists. See `crate::pipeline::RESERVED`
//! for why the first four and no others: they are the states nothing inside a
//! pipeline file can already put a `run:` step on, since none of them is a
//! step a pipeline may declare. `open` and `fetch` need no such reservation —
//! neither is a task stage, so nothing in a pipeline could ever collide with
//! either.
//!
//! This reuses [`crate::command_step::Runs`] rather than reinventing a second
//! way to spawn something detached and read its exit code back on a later
//! pass — a hook run is the same shape as a `run:` step's. It gets its own
//! directory, `tracking/` rather than `commands/`, so a project checking on a
//! stuck ticket integration never has to sift through build logs to find it.
//!
//! Firing is idempotent for a run's whole lifetime: [`fire`] starts a run
//! only while [`RunState::Fresh`] still holds. `queued`, `paused` and
//! `blocked` are stages a task can sit on for many passes in a row, which is
//! exactly what that guards: without it every pass sitting on `blocked`
//! would open a second ticket. The one caller that ever calls
//! [`Runs::forget`] is [`retry_if_failed`] — see there for why a `done` hold
//! is the one event this must not be true of forever. [`open_ticket`] needs
//! none of that: `queue add` is the only caller there ever is, and it means
//! "run this now" every time it calls at all — a document already naming a
//! `ticket:` is what its own caller, `queue::open_tickets`, reads as
//! "already open" and skips before this is ever reached.

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
    Some(checkout.join(".spoolway/hooks").join(named))
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
    ///   [`crate::config::check_id`]'s alphabet.
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
    let epic_body_path = dir.join(format!("{key}.epic-body.md"));
    let ticket_body_path = dir.join(format!("{key}.ticket-body.md"));
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
    std::fs::write(
        &epic_body_path,
        crate::task_template::render_tracking(
            &crate::task_template::resolve_tracking(repo, "epic"),
            &env,
        ),
    )?;
    std::fs::write(
        &ticket_body_path,
        crate::task_template::render_tracking(
            &crate::task_template::resolve_tracking(repo, "ticket"),
            &env,
        ),
    )?;
    env.insert("SPOOLWAY_OUT".to_string(), out_path.display().to_string());
    env.insert(
        "SPOOLWAY_EPIC_BODY".to_string(),
        epic_body_path.display().to_string(),
    );
    env.insert(
        "SPOOLWAY_TICKET_BODY".to_string(),
        ticket_body_path.display().to_string(),
    );

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
    ("SPOOLWAY_EVENT", "which of the six events this run is"),
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
    ("SPOOLWAY_SOURCE", "the task document's own `source:`"),
    ("SPOOLWAY_GROUP", "its `group:`"),
    (
        "SPOOLWAY_GROUP_DESCRIPTION",
        "its group's own `group_description:`, if one was set",
    ),
    ("SPOOLWAY_BRANCH", "its `branch:`"),
    ("SPOOLWAY_TITLE", "its title"),
    (
        "SPOOLWAY_TASK_FILE",
        "the document's path while this hook runs; blank when it has no file of its own",
    ),
    (
        "SPOOLWAY_GROUP_SIZE",
        "how many tasks this group is opening at once",
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
    (
        "SPOOLWAY_EPIC_BODY",
        "a file rendered from `.spoolway/templates/tracking/epic.md`",
    ),
    (
        "SPOOLWAY_TICKET_BODY",
        "a file rendered from `.spoolway/templates/tracking/ticket.md`",
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
/// that does not exist until after every document in the batch has opened
/// its ticket. The caller passes the path the document actually sits at
/// right now instead, which is what usually makes `SPOOLWAY_TASK_FILE` a
/// path a hook can open — usually, not always: a document with no file of
/// its own at all, such as a `queue add --from -` stream entry, has nothing
/// truthful to hand over, and the caller passes the empty string for that
/// case rather than a name nothing can open. `group_description` is
/// likewise the caller's to resolve — `open_tickets` reads it off whichever
/// document in the group set it,
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
/// any document naming the issue exists, so there is no task to key on.
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
/// before any document naming this issue even exists — so its tracking files
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
    let script = std::fs::read_to_string(checkout.join(".spoolway/hooks").join(name)).ok()?;
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
    let script = std::fs::read_to_string(checkout.join(".spoolway/hooks").join(name)).ok()?;
    (!has_fetch_branch(&script)).then(|| name.to_string())
}

/// Whether a hook is configured at all — what lets `queue add` skip
/// [`open_ticket`] and its own report entirely rather than call it once per
/// document only to have every call answer [`OpenResult::NoHook`].
pub fn configured(repo: &Repo) -> bool {
    hook_path(repo).is_some()
}

/// Whether `config.toml`'s `on_fail` asks a failed hook to hold the task
/// rather than only record the failure. Blank reads as `"ignore"`.
pub fn pauses_on_fail(repo: &Repo) -> bool {
    repo.config.issue_tracking.on_fail.trim() == "pause"
}

/// Whether a failed hook should actually hold a task at `queued` or `done` —
/// `on_fail = "pause"` *and* a hook that resolves to something real.
///
/// [`pauses_on_fail`] alone is not enough to gate a hold on: a blank `hook`,
/// or one [`is_bare_filename`] refuses, means [`fire`] never starts a run at
/// all, so [`exit_code`] can only ever read `None` for it. Holding on that
/// would deadlock every task at `queued` (and keep every one out of the
/// archive at `done`) for a project that asked for no hook at all — the
/// opposite of "an empty hook produces no hook runs and no behaviour change".
pub fn holds_on_fail(repo: &Repo) -> bool {
    pauses_on_fail(repo) && hook_path(repo).is_some()
}

/// Start this task's hook for `event`, unless it already has —see
/// [`RunState::Fresh`]. A no-op with nothing configured, so a project that
/// has never touched `[issue_tracking]` pays for none of this: no directory,
/// no process, no difference in behaviour.
///
/// `group_open` is how many of this task's own group are still in the open
/// set — [`crate::graph::Graph::group_open`]'s own count — which is both
/// `SPOOLWAY_GROUP_SIZE` and, on the `done` event, what decides
/// `SPOOLWAY_GROUP_LAST`: this task is still counted among the open ones at
/// the moment its own `done` hook fires, so a count of one means nobody else
/// is left.
pub fn fire(repo: &Repo, task: &Task, event: &str, group_open: usize) -> Result<()> {
    let Some(hook) = hook_path(repo) else {
        return Ok(());
    };
    let runs = runs(repo);
    let key = Runs::key(event, task.id());
    if runs.state(&key) != RunState::Fresh {
        return Ok(());
    }

    let env = build_env(repo, task, event, group_open);
    // Quoted the same way a lane's own environment is — see
    // `platform::quote` — so a checkout path holding a space still
    // reaches the shell as one argument.
    let run_line = crate::platform::quote(&hook.display().to_string());
    runs.start(&key, &run_line, &repo.root, &env)?;
    Ok(())
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

/// Where a key's last known failure is recorded once [`retry_if_failed`] has
/// forgotten the run that produced it — see there, and [`failure_count`],
/// for why the `.exit` file itself cannot be trusted to still be on disk by
/// the time anything goes looking for it.
fn failed_marker(repo: &Repo, key: &str) -> PathBuf {
    repo.tracking_dir().join(format!("{key}.failed"))
}

/// The road out of a `done` hold: forgets this task's `done` run once it has
/// failed, so the next pass's [`fire`] sees [`RunState::Fresh`] again and
/// starts it over rather than holding the task on the same stale exit code
/// forever.
///
/// `done` is not a step a person can resume the way `queued` failing into
/// `paused` can be — there is no later step to carry the task past, only the
/// same event to try again. Retrying the hook itself is what stands in for
/// "hold the task for `spoolway resume`" here: the task stays held, and each
/// pass gives the hook another chance rather than trusting a code it read
/// once. A run still in flight — [`RunState::Running`] or
/// [`RunState::Fresh`] — is left alone; forgetting it here would abandon a
/// process that might still succeed by dropping the very bookkeeping that
/// says it is going. `RunState::Interrupted` is retried too: a wrapper gone
/// without a code is not a verdict, and holding on that forever would be no
/// better than trusting a stale failure.
///
/// Leaves [`failed_marker`] behind before it forgets anything — `forget`
/// deletes the `.exit` file [`failure_count`] reads, and every pass either
/// forgets the failed run or restarts it, so without a separate record the
/// board's own count would go quiet about the one hook a person most needs
/// to see failing: the one stuck retrying forever.
pub fn retry_if_failed(repo: &Repo, task: &Task, event: &str) {
    let key = Runs::key(event, task.id());
    let runs = runs(repo);
    match runs.state(&key) {
        RunState::Exited(0) | RunState::Running | RunState::Fresh => {}
        RunState::Exited(_) | RunState::Interrupted => {
            let _ = std::fs::create_dir_all(repo.tracking_dir());
            let _ = std::fs::write(failed_marker(repo, &key), "");
            let _ = runs.forget(&key);
        }
    }
}

/// Clears the marker [`retry_if_failed`] may have left, once a held task's
/// hook has actually succeeded — called from the `done` arm's own success
/// path, so the board's count does not go on naming a failure that resolved.
pub fn clear_retry_marker(repo: &Repo, task: &Task, event: &str) {
    let key = Runs::key(event, task.id());
    let _ = std::fs::remove_file(failed_marker(repo, &key));
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

/// How many distinct task-and-event keys, across every one `tracking/`
/// holds, are currently failing. What the board's own
/// `issue_tracking: N hook failures` line counts.
///
/// Every failing key counts, not only the ones `on_fail = "pause"` is still
/// holding a task over — `"ignore"` records exactly the same failure, and a
/// person still wants to see it. Two kinds of evidence are read and merged
/// by key, since either may be the only one left for a given key: a `.exit`
/// file reading non-zero is a run [`retry_if_failed`] has not touched, and a
/// `.failed` marker is what survives one it has — retrying a `done` hold
/// forgets the run itself, `.exit` file included, on every pass that finds
/// it still failing, so without the marker that key would drop out of this
/// count the instant a retry began.
///
/// A `<task> · <event>` key whose task is no longer in the queue does not
/// count. [`reclaim`] already deletes those files when a task is archived;
/// this filter is what keeps the board honest when a task was archived by an
/// earlier build that did not, so one failed `open` hook does not read as
/// "1 hook failure" for the life of the project (review finding 64).
///
/// A `fetch` run's key names an issue reference, not a task (see
/// [`fetch_key`]) — it carries no `" · "` at all — so it is never subject to
/// that filter: a failed `spoolway issue show` still counts, and nothing
/// reclaims a `fetch` run file.
pub fn failure_count(repo: &Repo) -> usize {
    let Ok(entries) = std::fs::read_dir(repo.tracking_dir()) else {
        return 0;
    };
    let queued = repo.queued_ids();
    let mut failing: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Some(key) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        // Only a real `<task> · <event>` key is dropped once its task leaves
        // the queue; a separator-less `fetch-*` key belongs to no task and
        // stays.
        if let Some((task, _)) = key.split_once(" · ")
            && !queued.contains(task)
        {
            continue;
        }
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("failed") => {
                failing.insert(key);
            }
            Some("exit") => {
                let failed = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| raw.trim().parse::<i32>().ok())
                    .is_some_and(|code| code != 0);
                if failed {
                    failing.insert(key);
                }
            }
            _ => {}
        }
    }
    failing.len()
}

/// Every variable [`build_env`] adds beyond [`COMMON_EVENT_VARS`], for the
/// four events that fire once a task settles — see [`OPEN_EVENT_VARS`] for
/// why this lives beside the function it describes rather than in
/// `commands::hook`.
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

    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("tracking-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spoolway/hooks")).unwrap();
        Repo {
            checkout: root.clone(),
            root: root.clone(),
            config: Config::default(),
            home: root.join(".home"),
        }
    }

    /// Writes `.spoolway/hooks/<name>`, executable, and points the repo's
    /// `[issue_tracking]` at it.
    fn with_hook(repo: &mut Repo, name: &str, script: &str) {
        let path = repo.checkout.join(".spoolway/hooks").join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
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
            touches: Vec::new(),
            depends_on: Vec::new(),
            parallel: false,
            borrowed: false,
            last_report: None,
            blocked_from: None,
            parked_from: None,
            escalated: false,
            resume: None,
            pipeline: None,
            group: None,
            group_description: None,
            source: None,
            plan: None,
            gate_at: None,
            branch: None,
            base: None,
            run: None,
            cut_from: None,
            base_commit: None,
            patch: None,
            skip: Vec::new(),
            trial: None,
            replay_of: None,
            worktree_path: None,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            attempts: 0,
            paused_at: None,
            paused_by: None,
            launched_at: None,
            steps: Default::default(),
            rounds: Default::default(),
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

    /// The whole point of the empty-table default: a project that has never
    /// touched `[issue_tracking]` gets no directory, no process, nothing.
    #[test]
    fn a_blank_hook_fires_nothing() {
        let repo = fixture("blank");
        let t = task("demo", |_| {});
        fire(&repo, &t, crate::pipeline::QUEUED, 1).unwrap();
        assert!(!repo.tracking_dir().join("demo · queued.exit").exists());
        assert_eq!(exit_code(&repo, &t, crate::pipeline::QUEUED), None);
        assert_eq!(failure_count(&repo), 0);
    }

    /// The hook actually runs, with the environment the acceptance criteria
    /// name, and its own exit code is what a later pass reads back.
    #[test]
    fn a_configured_hook_runs_with_the_full_environment() {
        let mut repo = fixture("env");
        with_hook(&mut repo, "echo.sh", "env | sort; exit 3");
        // `failure_count` only counts a key whose task is still in the
        // queue — so the task has to actually be there.
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
        assert_eq!(failure_count(&repo), 1);

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
        ] {
            assert!(log.contains(expected), "missing `{expected}` in:\n{log}");
        }
        // Only `done` ever carries this, whatever the group's size.
        assert!(!log.contains("SPOOLWAY_GROUP_LAST"));
    }

    /// A failed hook from a task that has since left the queue stops
    /// counting, and [`reclaim`] takes its run files with it at archive time
    /// — so "1 hook failure" does not sit on the board for the life of the
    /// project (review finding 64).
    #[test]
    fn a_failure_from_an_archived_task_stops_showing_on_the_board() {
        let mut repo = fixture("archived-failure");
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
            failure_count(&repo),
            1,
            "a failing hook of a queued task counts"
        );

        // The task is archived: its file leaves the queue.
        std::fs::remove_file(repo.queue_dir().join("demo.md")).unwrap();
        assert_eq!(
            failure_count(&repo),
            0,
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

    /// A `fetch` run's key names an issue reference, not a task, so it carries
    /// no `" · "` and the queued-task filter leaves it alone — a failed
    /// `spoolway issue show` still shows on the board even with nothing in
    /// the queue.
    #[test]
    fn a_failed_fetch_still_counts_though_it_names_no_task() {
        let repo = fixture("fetch-counts");
        std::fs::create_dir_all(repo.tracking_dir()).unwrap();
        let key = fetch_key("https://github.com/o/r/issues/42");
        std::fs::write(repo.tracking_dir().join(format!("{key}.exit")), "9\n").unwrap();
        assert_eq!(failure_count(&repo), 1);
    }

    /// A hook fires once per task per event for the run's whole lifetime — a
    /// second call while the task still sits on the same event must not
    /// start a second process.
    #[test]
    fn a_hook_fires_once_per_task_per_event() {
        let mut repo = fixture("once");
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
        let repo = fixture("group-last");
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

        let repo = fixture("hook-contract-vars");
        let t = task("demo", |f| f.group = Some("g".into()));

        // `open_env` never sees `SPOOLWAY_OUT`, `SPOOLWAY_EPIC_BODY` or
        // `SPOOLWAY_TICKET_BODY` — `open_ticket` adds those three once the
        // tracking directory it points into exists — so they are added here
        // exactly the way that caller does, rather than expected of
        // `open_env` itself.
        let mut open = open_env(&repo, &t, 1, "", "", "", "demo.md");
        open.insert("SPOOLWAY_OUT".to_string(), String::new());
        open.insert("SPOOLWAY_EPIC_BODY".to_string(), String::new());
        open.insert("SPOOLWAY_TICKET_BODY".to_string(), String::new());
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
        // it the same way `open_ticket` adds its own three.
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

    /// `on_fail` reads `"pause"` as pausing and anything else — blank
    /// included — as `"ignore"`, exactly as the acceptance criteria say.
    #[test]
    fn on_fail_defaults_to_ignore() {
        let mut repo = fixture("on-fail");
        assert!(!pauses_on_fail(&repo));
        repo.config.issue_tracking.on_fail = "ignore".into();
        assert!(!pauses_on_fail(&repo));
        repo.config.issue_tracking.on_fail = "pause".into();
        assert!(pauses_on_fail(&repo));
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
        let mut repo = fixture("path-traversal");
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

    /// The road out of a `done` hold: a failed run is forgotten so the next
    /// pass's `fire` starts it over, and a run still going or already clean
    /// is left exactly as it is.
    #[test]
    fn retry_if_failed_only_forgets_a_completed_failure() {
        let mut repo = fixture("retry");
        with_hook(&mut repo, "flaky.sh", "exit 1");
        let t = task("demo", |_| {});

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        settle(&repo, &t, crate::pipeline::DONE);
        assert_eq!(exit_code(&repo, &t, crate::pipeline::DONE), Some(1));

        retry_if_failed(&repo, &t, crate::pipeline::DONE);
        assert_eq!(
            exit_code(&repo, &t, crate::pipeline::DONE),
            None,
            "forgotten, so there is no code to read until it runs again"
        );
        // And the next `fire` sees `Fresh` again and actually restarts it.
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
        let repo = fixture("open-no-hook");
        let t = task("demo", |_| {});
        assert_eq!(
            open_ticket(&repo, &t, 1, "", "", "", "demo.md").unwrap(),
            OpenResult::NoHook
        );
    }

    /// A hook that answers cleanly hands back exactly what it wrote to
    /// `SPOOLWAY_OUT`, and both body files it named in `SPOOLWAY_EPIC_BODY`
    /// / `SPOOLWAY_TICKET_BODY` actually exist and carry the substituted
    /// template, not the placeholder text.
    #[test]
    fn open_ticket_reads_the_answer_and_writes_both_rendered_bodies() {
        let mut repo = fixture("open-answers");
        // `resolve_tracking` never falls back to the shipped
        // `assets/tracking/epic.md` at render time any more — a project's
        // own file is the only thing it reads — so this writes one to
        // actually exercise the substitution rather than the single-line
        // fallback every project without one gets.
        std::fs::create_dir_all(repo.tracking_templates_dir()).unwrap();
        std::fs::write(
            repo.tracking_templates_dir().join("epic.md"),
            "Opened for group `${SPOOLWAY_GROUP}`. Plan: ${SPOOLWAY_SOURCE}\n",
        )
        .unwrap();
        with_hook(
            &mut repo,
            "open.sh",
            r#"cat "$SPOOLWAY_EPIC_BODY" >"$SPOOLWAY_EPIC_BODY.seen"
               cat "$SPOOLWAY_TASK_FILE" >"$SPOOLWAY_EPIC_BODY.task-file.seen"
               echo "$SPOOLWAY_GROUP_DESCRIPTION" >"$SPOOLWAY_EPIC_BODY.description.seen"
               { echo "epic=$SPOOLWAY_GROUP_SIZE-parents:$SPOOLWAY_DEPENDS_TICKETS"; \
                 echo "ticket=acme/app#43"; } >"$SPOOLWAY_OUT""#,
        );
        let t = task("scan-pending", |f| {
            f.group = Some("scanner-rework".into());
            f.source = Some("/plans/scanner-rework.html".into());
        });

        // The document this task came from, sitting wherever `queue add`
        // reads it from mid-flight — not `t.path`, which names where it
        // will land in the queue, a file that does not exist yet.
        let source_doc = repo.root.join("scan-pending.md");
        std::fs::write(&source_doc, "the document's own live contents\n").unwrap();

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
        let seen =
            std::fs::read_to_string(repo.tracking_dir().join(format!("{key}.epic-body.md.seen")))
                .unwrap();
        assert!(seen.contains("group `scanner-rework`"), "{seen}");
        assert!(seen.contains("/plans/scanner-rework.html"), "{seen}");

        // `SPOOLWAY_TASK_FILE` names a path the hook can actually open and
        // read — the document's real, current contents, not the queue path
        // `validate_batch` has merely decided on.
        let task_file_seen = std::fs::read_to_string(
            repo.tracking_dir()
                .join(format!("{key}.epic-body.md.task-file.seen")),
        )
        .unwrap();
        assert_eq!(task_file_seen, "the document's own live contents\n");

        let description_seen = std::fs::read_to_string(
            repo.tracking_dir()
                .join(format!("{key}.epic-body.md.description.seen")),
        )
        .unwrap();
        assert_eq!(description_seen.trim(), "Mirrors a group of scans.");
    }

    /// A hook that exits non-zero is `Failed`, carrying the code back rather
    /// than an answer — nothing in `SPOOLWAY_OUT` is trusted once the script
    /// itself said it did not finish cleanly.
    #[test]
    fn open_ticket_reports_a_failed_exit() {
        let mut repo = fixture("open-fails");
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
        let repo = fixture("fetch-no-hook");
        assert_eq!(fetch_issue(&repo, "57").unwrap(), FetchResult::NoHook);
    }

    /// A hook script that has never heard of `fetch` — every install's,
    /// before somebody adds the branch — is caught before it is ever run,
    /// not left to answer an empty issue that reads as a real one.
    #[test]
    fn fetch_issue_with_no_fetch_branch_runs_nothing() {
        let mut repo = fixture("fetch-unimplemented");
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
        let mut repo = fixture("fetch-answers");
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
        let mut repo = fixture("fetch-fails");
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
        let repo = fixture("missing-fetch");
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
        let repo = fixture("read-answer-slug");
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
        let repo = fixture("missing-slug");
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

    /// A `gh` stand-in for the `done`-branch tests below: it logs every
    /// invocation, answers `pr view`'s `--json url` shape from a file the
    /// test writes first, captures whatever `--body` argument `gh pr
    /// comment` and `gh issue comment` are each given, and can be told to
    /// fail any of its four calls independently — `stub/pr_view_fail` for
    /// the branch lookup, `stub/pr_comment_fail` for the marker comment,
    /// `stub/issue_edit_fail` for the label swap, `stub/issue_comment_fail`
    /// for the final "ready for review" comment — which is what the
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
esac
exit 0
"#;
        let path = bin_dir.join("gh");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    /// A fixture whose `.spoolway/hooks/github.sh` is the real shipped
    /// script — [`crate::assets::HOOK_SCRIPTS`], not a paraphrase of it —
    /// with one line prepended to put the stub `gh` above ahead of the
    /// (absent) real one on `PATH`. Returns the fixture, a `demo` task on
    /// `task/demo` naming `ticket` as given, and the `stub/` directory the
    /// test still needs to seed before calling [`fire`].
    fn github_done_fixture(name: &str, ticket: &str) -> (Repo, Task, PathBuf) {
        let mut repo = fixture(name);
        let stub = repo.root.join("stub");
        write_stub_gh(&stub.join("bin"));

        let real = crate::assets::HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| *known == "github.sh")
            .expect("github.sh is a shipped hook")
            .1;
        let script = format!("PATH=\"{}:$PATH\"\n{real}", stub.join("bin").display());
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
        (repo, t, stub)
    }

    /// Acceptance criterion: `done` leaves the ticket open, marks the pull
    /// request with the marker `.github/workflows/spoolway-issues.yml`
    /// trusts, relabels the ticket for review, and closes nothing itself —
    /// closing is that workflow's job, once the pull request actually
    /// merges.
    #[test]
    fn github_sh_done_hands_the_ticket_to_its_pull_request_without_closing_it() {
        let (repo, t, stub) =
            github_done_fixture("done-handoff", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(0)
        );

        let marker = std::fs::read_to_string(stub.join("pr_comment.received")).unwrap();
        assert!(
            marker.contains("<!-- spoolway-issue: https://github.com/o/r/issues/12 -->"),
            "marker missing: {marker}"
        );
        let edit = std::fs::read_to_string(stub.join("issue_edit.log")).unwrap();
        assert!(
            edit.contains("--remove-label spoolway:in-progress"),
            "{edit}"
        );
        assert!(edit.contains("--add-label spoolway:review"), "{edit}");
        let ready = std::fs::read_to_string(stub.join("issue_comment.received")).unwrap();
        assert!(
            ready.contains("ready for review in https://github.com/o/r/pull/9"),
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
        let (repo, t, stub) =
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
    }

    /// The same "never silently skip" rule applies when the lookup succeeds
    /// but finds nothing: a `done` this hook was fired for always has a
    /// pull request behind it, so no result is as much a failure as a
    /// nonzero exit is.
    #[test]
    fn github_sh_done_fails_when_no_pull_request_is_found() {
        let (repo, t, stub) = github_done_fixture("done-no-pr", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(!stub.join("pr_comment.log").exists());
    }

    /// A failed marker comment must stop the handoff outright — the ticket
    /// is not yet relabelled or told anything, so nothing here claims a
    /// handoff `.github/workflows/spoolway-issues.yml` cannot yet see.
    #[test]
    fn github_sh_done_stops_when_the_marker_comment_fails() {
        let (repo, t, stub) =
            github_done_fixture("done-marker-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();
        std::fs::write(stub.join("pr_comment_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(!stub.join("issue_edit.log").exists());
        assert!(!stub.join("issue_comment.log").exists());
    }

    /// A failed label swap must stop before the final "ready for review"
    /// comment — the marker is already posted by this point (see
    /// `hand_off_for_review`'s own doc: it is written first on purpose), but
    /// nothing downstream of the failed call runs.
    #[test]
    fn github_sh_done_stops_when_the_label_swap_fails() {
        let (repo, t, stub) =
            github_done_fixture("done-label-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();
        std::fs::write(stub.join("issue_edit_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(stub.join("pr_comment.log").exists());
        assert!(!stub.join("issue_comment.log").exists());
    }

    /// Review finding, ported: a failed final comment used to be swallowed
    /// by the done branch's own unconditional success, so the hook could
    /// exit clean without ever describing the handoff as the acceptance
    /// criteria require. The marker and the label swap have already landed
    /// by this point, so a retry only needs to redo the one comment that
    /// failed.
    #[test]
    fn github_sh_done_fails_when_the_final_comment_fails() {
        let (repo, t, stub) =
            github_done_fixture("done-comment-fails", "https://github.com/o/r/issues/12");
        std::fs::write(stub.join("pr_url"), "https://github.com/o/r/pull/9\n").unwrap();
        std::fs::write(stub.join("issue_comment_fail"), "").unwrap();

        fire(&repo, &t, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(
            settle(&repo, &t, crate::pipeline::DONE),
            RunState::Exited(1)
        );

        assert!(stub.join("pr_comment.log").exists());
        assert!(stub.join("issue_edit.log").exists());
    }
}
