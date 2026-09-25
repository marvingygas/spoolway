//! The task-document contract, and checking a document against it before it
//! is queued.
//!
//! `spoolway task contract` is the whole interface a producer needs. Printed
//! bare, it is every key a document may set, every key it may not, a
//! sentence on how to size a breakdown, and — per pipeline — its longest
//! agent step, the steps `gate_at` accepts, its own `description:`, which step is
//! `last-of-chain`, and the
//! body skeleton itself; under `output` it is the directory a finished
//! document is written to, what to name it there, and the two commands that
//! check it and send it. One call, so a producer never has to be told
//! anything a person read somewhere else: a model handed this JSON and a
//! goal has everything it needs to leave a queueable task on disk. Run with
//! `--from`, it is the exact validation `queue add --from` runs, with
//! nothing written at the end of it.
//!
//! Both modes are read straight off [`super::queue::RESERVED_KEYS`],
//! [`super::queue::longest_agent_step`], [`super::queue::gather_documents`]
//! and [`super::queue::validate_batch`] — the functions that actually
//! enforce the contract — so this can never say something the enforcement
//! does not, or the other way round.

use super::*;

/// Keys a document must set — refused by `parse_submission` when blank or
/// absent.
const REQUIRED_KEYS: &[&str] = &["id", "title", "group", "pipeline"];

/// Keys a document may set, and spoolway keeps exactly what it wrote.
const OPTIONAL_KEYS: &[&str] = &[
    "source",
    "plan",
    "touches",
    "depends_on",
    "parallel",
    "gate_at",
    "epic",
    "ticket",
    "base",
    "group_description",
];

/// The keys spoolway's own dispatcher machinery throws away unconditionally
/// once a document reaches `parse_submission`, whatever value the document
/// gave them. Distinct from [`super::queue::RESERVED_KEYS`]: setting one of
/// *these* is not refused, it is simply thrown away, because a document
/// cannot know the run id, the worktree path or the launch counters before
/// any of them exist.
///
/// Most of these are overwritten by one of the `front.<field> = ...`
/// assignments in `parse_submission` — everything but the five quota-park
/// keys at the end, which have no `Frontmatter` field left to assign: a
/// document still carrying one of those five is a task file that predates
/// their retirement, and `RETIRED_PARK_KEYS` (`src/task.rs`) strips it out
/// of `extra` instead, the same way `Task::parse` drops one already queued.
const IGNORED_KEYS: &[&str] = &[
    "borrowed",
    "last_report",
    "blocked_from",
    "parked_from",
    "escalated",
    "resume",
    "patch",
    "skip",
    "replay_of",
    "worktree_path",
    "workspace_id",
    "pane_id",
    "tab_id",
    "paused_at",
    "paused_by",
    "launched_at",
    "steps",
    "rounds",
    "launch_failures",
    "launch_busy_since",
    "arrived_from",
    "usage_limit_hold",
    "quota_retries",
    "parked_until",
    "parked_window",
    "parked_at",
];

/// What a document does with a key none of the four groups above name.
const PASSTHROUGH: &str = "any key not named here survives untouched, for a project's own metadata";

/// One sentence per settable key — [`REQUIRED_KEYS`] and [`OPTIONAL_KEYS`],
/// together — on how to fill it. Nothing here about sizing, non-goals or
/// acceptance criteria: that guidance belongs to the body skeleton, which
/// this same call ships under `pipelines.<name>.body`.
const FIELD_SENTENCES: &[(&str, &str)] = &[
    (
        "id",
        "A short, unique name for this task — becomes the filename, the branch \
         suffix and half a lane name, so it holds only lowercase letters, digits \
         and hyphens.",
    ),
    (
        "title",
        "A Conventional Commits line: a type, the area of code in parentheses, a \
         colon, and one short present-tense sentence — `feat(queue): add a \
         --dry-run flag`. The type is one of `feat`, `fix`, `docs`, `refactor`, \
         `perf`, `test`, `build`, `ci` or `chore`; the parentheses come off when \
         no single area fits. Used verbatim — as the squashed commit's subject \
         and the pull request's title where nothing else sets one.",
    ),
    (
        "group",
        "The chain of work this task belongs to, read verbatim — required, since \
         a lane runs in the tab its group shares with its siblings.",
    ),
    (
        "source",
        "Where this task came from — an issue URL, a plan page path, anything — \
         kept but never parsed. The issue a planning skill read through `spoolway \
         issue show`, whenever there was one; a plan page's own path only when \
         there was no issue behind it.",
    ),
    (
        "plan",
        "The plan page that argued this task's shape, as an absolute path — set \
         only when a page was written and an issue also holds `source:`, since a \
         page with no issue behind it keeps its own path in `source:` instead. \
         Kept but never parsed, and never required.",
    ),
    (
        "touches",
        "Glob patterns this task is expected to modify, used for conflict \
         detection and reviewer-effort sizing.",
    ),
    (
        "depends_on",
        "Task ids that must finish before this one may start.",
    ),
    (
        "parallel",
        "Set true only when a `touches` overlap with another task of the same \
         group is deliberate, not a missing `depends_on`.",
    ),
    (
        "pipeline",
        "Which pipeline to run this task on — required, and must name one of \
         the pipelines below.",
    ),
    (
        "gate_at",
        "A step id to pause the task on once it passes, for a person to \
         `spoolway resume` before it continues.",
    ),
    (
        "epic",
        "The `[issue_tracking]` open hook's own epic id for this task's group, \
         opaque and never parsed — write it by hand only when resuming a batch \
         a failed hook already opened one for.",
    ),
    (
        "ticket",
        "The `[issue_tracking]` open hook's own ticket id for this task, opaque \
         and never parsed. This is the key an inbound producer sets to hand a \
         document a ticket it already knows about, rather than have `open` mint \
         one — and, the same way, the resume path a failed batch leaves behind: a \
         document that already sets this is reported `kept` at `queue add` and \
         the hook never runs for it.",
    ),
    (
        "base",
        "The branch this task is cut from and merges back into — must name a \
         branch the repository already has locally. Leave it unset to take the \
         submission's own `queue add --base` instead; a submission that sets \
         neither is refused, naming the document.",
    ),
    (
        "group_description",
        "The group's own words for the issue a mirror opens above it — carried \
         verbatim into the open hook's `SPOOLWAY_GROUP_DESCRIPTION`. Only one \
         document of a group needs to set it; a submission is refused, naming \
         the group, when a hook is configured and none of the group's documents \
         set it. Never required when no hook is configured.",
    ),
];

/// Rules `check_dependencies_set` and `validate_batch` enforce over a whole
/// submission, not any one document alone — worded for a reader with no
/// access to either function's source.
fn set_rules() -> Vec<String> {
    vec![
        "no two documents in one submission may share an `id`, and none may name \
         an `id` already queued"
            .to_string(),
        "a `depends_on` may not name the document's own `id`".to_string(),
        "a `depends_on` must name a task already in the queue or the archive, or a \
         document in this same submission — not one queued nowhere at all"
            .to_string(),
        "a `depends_on` may not close a cycle, however many hops long".to_string(),
        "a dependency and its dependent must share the same `base` — a dependent's \
         worktree is cut from its dependency's branch, and that only ever merges \
         back into its own group's base"
            .to_string(),
        "when `issue_tracking.hook` is configured, every group must set \
         `group_description:` on at least one of its documents — never required \
         with no hook configured"
            .to_string(),
    ]
}

/// Where a finished document goes, and what happens to it there — the half
/// of the contract that is not about a document's own content.
///
/// Read off [`Repo::pending_dir`] rather than spelled out as
/// `~/.spoolway/<project>/pending`, so a producer is handed the path this
/// machine actually resolves instead of a pattern it has to expand itself.
#[derive(Debug, serde::Serialize)]
struct ContractOutput {
    dir: String,
    filename: &'static str,
    verify: String,
    queue: &'static str,
}

/// One pipeline's own slice of the contract: its longest agent step, the
/// steps a `gate_at` may name, which of them are `last-of-chain` and
/// `first-of-chain`, this pipeline's own `description:`, and the body a task
/// on it is written from.
///
/// `longest_agent_step` no longer bounds a task id's length — see gh-359 and
/// [`crate::mux::check_task_id`] — but is kept here as information: a lane
/// name too long for herdr's own wire spelling still gets a short internal
/// alias rather than a name change, and a person sizing a task id may still
/// want to know which step it will run longest against.
#[derive(Debug, serde::Serialize)]
struct PipelineContract {
    longest_agent_step: String,
    gate_at: Vec<String>,
    /// The step a `run:` chain treats as the top of the chain — `None` when
    /// this pipeline marks no step `last: true`, the same case `pipeline
    /// show` renders by saying nothing rather than printing a placeholder.
    last_of_chain: Option<String>,
    /// The step only a chain's declared root runs — `None` when this pipeline
    /// marks no step `first: true`. Separate from `last_of_chain` because a
    /// pipeline may mark both, on different steps, and a producer sizing a
    /// chain needs to know which end each one lands on.
    first_of_chain: Option<String>,
    description: Option<String>,
    body: String,
}

#[derive(Debug, serde::Serialize)]
struct ContractKeys {
    required: &'static [&'static str],
    optional: &'static [&'static str],
    refused: &'static [&'static str],
    ignored: &'static [&'static str],
    passthrough: &'static str,
}

/// One sentence on how to size a breakdown — what `spoolway-tasks` used to
/// work out itself from a pipeline's model windows, before there was
/// anywhere to print it instead. Judgment, not arithmetic: see this task's
/// own non-goals for why no per-pipeline figure replaces it.
const SIZING: &str = "Cut a reasonable number of tasks for the shape at hand, each routed to \
                      one of the pipelines below. No arithmetic: judge the split by subject, \
                      and keep each task's criteria under five bullets.";

/// The whole task-document contract, printed as JSON by bare `spoolway task
/// contract`.
#[derive(Debug, serde::Serialize)]
struct Contract {
    sizing: &'static str,
    output: ContractOutput,
    keys: ContractKeys,
    set_rules: Vec<String>,
    fields: std::collections::BTreeMap<&'static str, &'static str>,
    pipelines: std::collections::BTreeMap<String, PipelineContract>,
}

fn build_contract(repo: &Repo, pipelines: &Pipelines) -> Contract {
    let pipelines_out = pipelines
        .pipelines
        .iter()
        .map(|(name, pipeline)| {
            let longest = super::queue::longest_agent_step(pipeline);
            let body = crate::task_template::resolve(repo, pipeline.task_template_name());
            let last_of_chain = pipeline
                .steps
                .iter()
                .find(|step| step.last)
                .map(|step| step.id.clone());
            let first_of_chain = pipeline
                .steps
                .iter()
                .find(|step| step.first)
                .map(|step| step.id.clone());
            (
                name.clone(),
                PipelineContract {
                    longest_agent_step: longest.to_string(),
                    gate_at: pipeline.steps.iter().map(|s| s.id.clone()).collect(),
                    last_of_chain,
                    first_of_chain,
                    description: pipeline.description.clone(),
                    body,
                },
            )
        })
        .collect();

    let pending = repo.pending_dir();
    let pending = pending.display().to_string();

    Contract {
        sizing: SIZING,
        output: ContractOutput {
            verify: format!("spoolway task contract --from {pending}"),
            dir: pending,
            filename: "<id>.md, one document per file — this directory holds task \
                       documents and nothing else",
            queue: "spoolway queue — writing a document does not queue it; a person \
                    selects a group there and sends it",
        },
        keys: ContractKeys {
            required: REQUIRED_KEYS,
            optional: OPTIONAL_KEYS,
            refused: super::queue::RESERVED_KEYS,
            ignored: IGNORED_KEYS,
            passthrough: PASSTHROUGH,
        },
        set_rules: set_rules(),
        fields: FIELD_SENTENCES.iter().copied().collect(),
        pipelines: pipelines_out,
    }
}

/// Bare `spoolway task contract`: the contract, and nothing else on stdout —
/// see the acceptance criterion this exists to satisfy.
fn print_contract(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&build_contract(repo, pipelines))?
    );
    Ok(())
}

/// `spoolway task contract --from`, once every document has passed: the same
/// four things a person would otherwise have to read the code to know were
/// checked, one line each, for every document in the batch.
fn print_check_report(tasks: &[Task], repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    let existing = repo.tasks()?;

    for task in tasks {
        if tasks.len() > 1 {
            println!("{}", task.id());
        }

        let pipeline_name = task
            .front
            .pipeline
            .as_deref()
            .expect("validate_batch refuses a document with no `pipeline:`");
        // Only checked for existing — `validate_batch` runs the same check
        // before ever writing a document, and this report exists to say what
        // that check already confirmed, not to run a second one.
        pipelines.get(pipeline_name)?;

        // The same overlap `queue conflicts` reports, against the queue as
        // it stands today — advisory, like that command, and never a reason
        // for this to exit non-zero: `queue add --from` does not refuse an
        // overlap either, so neither does this.
        let overlapping: Vec<&str> = existing
            .iter()
            .filter(|other| {
                task.front.touches.iter().any(|glob| {
                    other
                        .front
                        .touches
                        .iter()
                        .any(|theirs| crate::globs::overlaps(glob, theirs))
                })
            })
            .map(Task::id)
            .collect();
        let touches = match overlapping.is_empty() {
            true => format!(
                "{} globs, overlapping nothing already queued",
                task.front.touches.len()
            ),
            false => format!(
                "{} globs, overlapping {} already queued",
                task.front.touches.len(),
                overlapping.join(", ")
            ),
        };

        let rows = [
            (
                "frontmatter",
                "id, title, group and the body are present, and no key spoolway owns \
                 itself is set"
                    .to_string(),
            ),
            ("id", format!("`{}` is a path-safe task id", task.id())),
            (
                "depends_on",
                "every name resolves, in this set or in the queue".to_string(),
            ),
            ("touches", touches),
        ];
        for (label, text) in rows {
            println!("  {label:<12} {text}");
        }
        println!();
    }

    println!("no problems — nothing written");
    Ok(())
}

/// `spoolway task contract`: the contract itself with no arguments, or a
/// document checked against it with `--from` — the same validation `queue
/// add --from` runs, [`super::queue::validate_batch`] and all, with nothing
/// saved at the end of it. A refusal propagates exactly as `queue add
/// --from`'s own does, which is what gives the two the same wording: this is
/// not a second message written to match, it is the one error value both
/// commands return.
pub fn task_contract(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &TaskContractArgs,
    // No longer read for a base: see `queue_add`'s own `_cwd` for why this
    // stays in the signature unused rather than pulled from every call site.
    _cwd: &std::path::Path,
) -> Result<()> {
    if args.from.is_empty() {
        return print_contract(repo, pipelines);
    }

    let documents = super::queue::gather_documents(&args.from)?;
    let tasks = super::queue::validate_batch(repo, pipelines, args.base.as_deref(), &documents)?;
    print_check_report(&tasks, repo, pipelines)
}

/// `spoolway task edit`: rewrite one section of a stopped task's document,
/// under its task lock.
///
/// A stopped task belongs to whoever is looking at its pane — see
/// `compose::situating`'s own bullet to that effect — and this is the tool
/// that makes good on it: nothing else in spoolway lets a lane, or a person
/// working alongside one, rewrite a queued task's body. Refused outright
/// against anything still moving, so a lane cannot use it to edit around
/// `spoolway report`'s own contract; a task on `paused` or `blocked` is not
/// moving until somebody who has read it says so.
///
/// Locked the same best-effort way [`super::report`] takes the lock for a
/// report: a lock a live process still holds after the wait is logged, and
/// the edit proceeds unlocked rather than being refused outright — the
/// person reading a stopped pane is exactly the reader this exists for, and
/// making them retry a benign race is a worse failure than the rare lost
/// update this guards against.
pub fn task_edit(repo: &Repo, args: &TaskEditArgs) -> Result<()> {
    crate::config::check_id("task id", &args.task)?;

    let task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(&args.task));
    if task_lock.is_err() {
        crate::problem_log::append(
            repo,
            &format!("{}: task lock still held, editing without it", args.task),
        );
    }

    let mut task = repo.task(&args.task)?;
    let stage = task.stage();
    if stage != crate::pipeline::PAUSED && stage != crate::pipeline::BLOCKED {
        bail!(
            "task `{}` is at `{stage}`, not `{}` or `{}` — only a stopped task's document may \
             be rewritten this way.",
            args.task,
            crate::pipeline::PAUSED,
            crate::pipeline::BLOCKED
        );
    }

    let heading = format!("## {}", args.section);
    let content = read_section_content(&args.from)?;
    task.replace_section(&heading, &content)?;
    task.save()?;

    // The one action left once an edit lands: resuming, and only resuming —
    // `refuse_from_lane` still means a lane cannot type this itself, but the
    // person reading this pane can, and it is named key first, then the
    // command, exactly as every other choice a stop offers is now — see
    // `commands::report::stop_choices`, printed at the moment a report first
    // parks a task here rather than every time its document changes.
    println!(
        "{}: `{heading}` rewritten, {} lines\n\n  resuming it is still a person's:\n  resume   \
         [r]   spoolway resume {}",
        args.task,
        content.lines().count(),
        args.task
    );
    Ok(())
}

/// `--from`'s content: a file, or `-` for standard input — the same two
/// shapes `queue add --from` reads a document from, minus the directory and
/// stream-of-documents cases neither makes sense for one section.
fn read_section_content(from: &str) -> Result<String> {
    if from == "-" {
        let mut body = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
            .context("reading the new section from standard input")?;
        return Ok(body);
    }
    std::fs::read_to_string(from).with_context(|| format!("reading {from}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::*;

    /// A minimal, non-empty body — the shape most of these tests only need
    /// to exist, not to say anything in particular.
    const BODY: &str = "## Goal\n\nDo the thing.\n";

    /// A whole task document, in the shape `--from` accepts: `id:` plus
    /// whatever else `extra` puts in the frontmatter, then `body`.
    /// `pipeline:` is required now, so this fills in the built-in `default`
    /// pipeline unless `extra` already names one.
    fn document(id: &str, extra: &str, body: &str) -> String {
        let pipeline = if extra.contains("pipeline:") {
            ""
        } else {
            "pipeline: default\n"
        };
        format!("---\nid: {id}\ntitle: {id}, done\n{pipeline}{extra}---\n{body}")
    }

    /// Write `text` under `repo.root` and hand back the path a `--from`
    /// entry would name.
    fn write_doc(repo: &Repo, name: &str, text: &str) -> String {
        let path = repo.root.join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn contract_args(paths: &[&str]) -> TaskContractArgs {
        TaskContractArgs {
            from: paths.iter().map(|p| p.to_string()).collect(),
            base: Some("plan/demo".to_string()),
        }
    }

    fn from_args(paths: &[&str]) -> QueueAddArgs {
        QueueAddArgs {
            from: paths.iter().map(|p| p.to_string()).collect(),
            base: None,
            dry_run: false,
        }
    }

    /// Every public field [`crate::task::Frontmatter`] declares has to show up
    /// in one of the four groups bare `task contract` prints — read off
    /// `task.rs`'s own source rather than a hand-copied list, so a field added
    /// there and forgotten here fails the build instead of silently going
    /// missing from the contract.
    #[test]
    fn every_frontmatter_field_is_in_some_contract_group() {
        let repo = fixture("contract-groups");
        let contract = build_contract(&repo, &Pipelines::builtin());
        let mut known = std::collections::BTreeSet::new();
        known.extend(contract.keys.required.iter().copied());
        known.extend(contract.keys.optional.iter().copied());
        known.extend(contract.keys.refused.iter().copied());
        known.extend(contract.keys.ignored.iter().copied());

        let fields = frontmatter_field_names();
        assert!(!fields.is_empty(), "found no fields to check at all");
        for field in fields {
            // `extra` is not a document key at all — it is the map every
            // unrecognised key round-trips through, which `keys.passthrough`
            // already describes in prose rather than by name.
            if field == "extra" {
                continue;
            }
            assert!(
                known.contains(field),
                "`{field}` is a public field of Frontmatter but is in none of \
                 task_contract's contract groups — add it to one"
            );
        }
    }

    /// A key a document may actually set — `required` or `optional` — has to
    /// carry one sentence in `fields` on how to fill it, read off the same
    /// contract rather than a hand-copied list: a key added to one group and
    /// forgotten in the other fails the build the same way a forgotten
    /// `Frontmatter` field does above.
    #[test]
    fn every_settable_key_has_a_fields_entry() {
        let repo = fixture("contract-fields");
        let contract = build_contract(&repo, &Pipelines::builtin());
        let mut settable = std::collections::BTreeSet::new();
        settable.extend(contract.keys.required.iter().copied());
        settable.extend(contract.keys.optional.iter().copied());

        for key in settable {
            assert!(
                contract.fields.contains_key(key),
                "`{key}` may be set by a document but has no `fields` entry — add one"
            );
        }
    }

    /// The name of every field declared directly on `pub struct Frontmatter`
    /// in `task.rs`, in source order.
    fn frontmatter_field_names() -> Vec<&'static str> {
        const SRC: &str = include_str!("../task.rs");
        let struct_start = SRC
            .find("pub struct Frontmatter {\n")
            .expect("`pub struct Frontmatter {` not found in task.rs");
        // Past the declaration line itself, which would otherwise read as a
        // field named `struct Frontmatter {`.
        let body = &SRC[struct_start + "pub struct Frontmatter {\n".len()..];
        let struct_end = body
            .find("\n}")
            .expect("Frontmatter's closing brace not found");
        let body = &body[..struct_end];

        body.lines()
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|rest| rest.split(':').next())
            .map(str::trim)
            .collect()
    }

    /// The sections the acceptance criteria names, all present, and the
    /// JSON on stdout with nothing else beside it.
    #[test]
    fn bare_contract_prints_only_the_contract_as_json() {
        let repo = fixture("contract-bare");
        let value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&build_contract(&repo, &Pipelines::builtin())).unwrap(),
        )
        .unwrap();
        assert!(
            value.get("default").is_none(),
            "no project default to advertise: {value}"
        );
        assert!(value["sizing"].is_string(), "{value}");
        assert!(value.get("keys").is_some());
        assert!(value.get("fields").is_some());
        assert!(value.get("pipelines").is_some());
        assert!(value.get("set_rules").is_some());
        assert!(value["keys"]["passthrough"].is_string(), "{value}");
        assert!(
            value["pipelines"]["default"]["longest_agent_step"].is_string(),
            "{value}"
        );
        assert!(
            !value["pipelines"]["default"]
                .as_object()
                .unwrap()
                .contains_key("id_budget"),
            "gh-359: no pipeline-dependent task-id budget to advertise: {value}"
        );
        assert!(
            value["pipelines"]["default"]["gate_at"].is_array(),
            "{value}"
        );
        assert!(value["pipelines"]["default"]["body"].is_string(), "{value}");
        assert!(
            !value["pipelines"]["default"]
                .as_object()
                .unwrap()
                .contains_key("window"),
            "no per-pipeline `window` — a caller judges sizing by subject, not arithmetic: {value}"
        );
    }

    /// The `title` field names every Conventional Commits type this project
    /// actually uses — `assets/skills/*/spoolway-tasks/SKILL.md` hardcodes
    /// the same nine today, and this is the one place a caller should be
    /// able to read them from instead.
    #[test]
    fn title_field_names_all_nine_commit_types() {
        let repo = fixture("contract-title-types");
        let contract = build_contract(&repo, &Pipelines::builtin());
        let title = contract.fields["title"];
        for kind in [
            "feat", "fix", "docs", "refactor", "perf", "test", "build", "ci", "chore",
        ] {
            assert!(
                title.contains(kind),
                "`title` should name the `{kind}` commit type: {title}"
            );
        }
    }

    /// Every pipeline carries its own `description:` and `last_of_chain` —
    /// the two facts a caller otherwise has to open `pipeline show` for.
    /// `last_of_chain` is `None` for a pipeline that marks no step
    /// `last: true`, and the step's own id for one that does.
    #[test]
    fn pipeline_contract_carries_description_and_last_of_chain() {
        let mut pipelines = Pipelines::builtin();
        let with_last = pipelines.pipelines.get_mut("default").unwrap();
        with_last.description = Some("a test pipeline".to_string());
        let last_step_id = with_last.steps.first().unwrap().id.clone();
        with_last.steps.first_mut().unwrap().last = true;

        let repo = fixture("contract-last-of-chain");
        let contract = build_contract(&repo, &pipelines);

        let default_out = &contract.pipelines["default"];
        assert_eq!(default_out.description.as_deref(), Some("a test pipeline"));
        assert_eq!(
            default_out.last_of_chain.as_deref(),
            Some(last_step_id.as_str())
        );

        // `bugfix` marks no step `last: true` in this fixture — `None`
        // rather than a made-up placeholder.
        let bugfix_out = &contract.pipelines["bugfix"];
        assert_eq!(bugfix_out.last_of_chain, None);
    }

    /// `first_of_chain` names the step only a chain's declared root runs, the
    /// same way `last_of_chain` names the one only its top runs. A pipeline
    /// may mark both, on different steps, so the two fields are read
    /// independently and neither shadows the other.
    #[test]
    fn pipeline_contract_carries_first_of_chain_beside_last_of_chain() {
        let mut pipelines = Pipelines::builtin();
        let with_both = pipelines.pipelines.get_mut("default").unwrap();
        let first_step_id = with_both.steps.first().unwrap().id.clone();
        let last_step_id = with_both.steps.last().unwrap().id.clone();
        with_both.steps.first_mut().unwrap().first = true;
        with_both.steps.last_mut().unwrap().last = true;

        let repo = fixture("contract-first-of-chain");
        let contract = build_contract(&repo, &pipelines);

        let default_out = &contract.pipelines["default"];
        assert_eq!(
            default_out.first_of_chain.as_deref(),
            Some(first_step_id.as_str())
        );
        assert_eq!(
            default_out.last_of_chain.as_deref(),
            Some(last_step_id.as_str())
        );

        // `bugfix` marks no step `first: true` in this fixture — `None`
        // rather than a made-up placeholder, the same as `last_of_chain`.
        let bugfix_out = &contract.pipelines["bugfix"];
        assert_eq!(bugfix_out.first_of_chain, None);
    }

    /// The contract names the directory a document is written to, and names
    /// the one this machine actually resolves — a producer that writes where
    /// `output.dir` says has written where `spoolway queue` reads, with no
    /// path pattern for it to expand on its own.
    #[test]
    fn the_contract_names_the_directory_documents_are_written_to() {
        let repo = fixture("contract-output");
        let contract = build_contract(&repo, &Pipelines::builtin());

        assert_eq!(
            std::path::Path::new(&contract.output.dir),
            repo.pending_dir()
        );
        assert!(
            contract.output.verify.contains(&contract.output.dir),
            "the check command has to name the directory: {}",
            contract.output.verify
        );
    }

    /// A document `queue add --from` would accept is reported with no
    /// problems, and nothing is written — the whole point of the command.
    #[test]
    fn from_accepts_a_good_document_and_writes_nothing() {
        let repo = fixture("check-good");
        let text = document(
            "checked",
            "group: demo\ntouches: [notes/checked.md]\n",
            BODY,
        );
        let path = write_doc(&repo, "checked.md", &text);

        assert!(
            task_contract(
                &repo,
                &Pipelines::builtin(),
                &contract_args(&[&path]),
                &repo.root
            )
            .is_ok()
        );
        assert!(
            !repo.queue_dir().join("checked.md").exists(),
            "a check must never write the document it validated"
        );
    }

    /// `task contract --from` runs `queue add --from`'s own base rule, not
    /// an ambient one of its own: a document with no `base:` and no
    /// `--base` is refused here exactly as `queue add` would refuse it,
    /// rather than checking out clean because this command reads the
    /// checkout's branch instead.
    #[test]
    fn from_refuses_a_document_with_no_base_and_no_flag() {
        let repo = fixture("check-no-base");
        let text = document("checked", "group: demo\n", BODY);
        let path = write_doc(&repo, "checked.md", &text);

        let err = task_contract(
            &repo,
            &Pipelines::builtin(),
            &TaskContractArgs {
                from: vec![path.clone()],
                base: None,
            },
            &repo.root,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no `base:`"), "{err:#}");

        assert!(
            task_contract(
                &repo,
                &Pipelines::builtin(),
                &TaskContractArgs {
                    from: vec![path],
                    base: Some("plan/demo".to_string()),
                },
                &repo.root,
            )
            .is_ok(),
            "--base covers it exactly as queue add --base would"
        );
    }

    /// The same refusal `queue add --from` gives for a document setting a
    /// reserved key, word for word — because this reaches the very same
    /// `parse_submission` rather than a second copy of its checks.
    #[test]
    fn from_refuses_a_reserved_key_the_same_way_add_does() {
        // One repo and one path for both calls: `contract --from` writes
        // nothing, so `add` afterwards sees the very same document at the
        // very same path, and the two error strings can be compared for
        // real — a second fixture would only ever differ by its own temp
        // path.
        let repo = fixture("check-reserved");
        let text = document("bad", "group: demo\nrun: r00001\n", BODY);
        let path = write_doc(&repo, "bad.md", &text);

        let check_err = task_contract(
            &repo,
            &Pipelines::builtin(),
            &contract_args(&[&path]),
            &repo.root,
        )
        .unwrap_err();
        assert!(
            !repo.queue_dir().join("bad.md").exists(),
            "a refused check must not write anything either"
        );

        let add_err = super::super::queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert_eq!(format!("{check_err:#}"), format!("{add_err:#}"));
    }

    /// The whole point: a stopped task's document is rewritten in place, and
    /// the rest of the file — frontmatter, every other section — survives
    /// untouched.
    #[test]
    fn task_edit_rewrites_a_paused_tasks_named_section() {
        let repo = fixture("edit-paused");
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.append_to_section("## Handoff", "keep me\n");
        task.save().unwrap();

        let from = write_doc(&repo, "mockup.txt", "line one\nline two\nline three\n");
        task_edit(
            &repo,
            &TaskEditArgs {
                task: "ship".into(),
                section: "Goal".into(),
                from,
            },
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(
            task.section("## Goal").unwrap(),
            "line one\nline two\nline three"
        );
        assert_eq!(
            task.section("## Handoff").unwrap(),
            "keep me",
            "a section this edit did not name is untouched"
        );
        assert_eq!(task.stage(), crate::pipeline::PAUSED, "still paused");
    }

    /// Nothing still moving may be rewritten this way — the boundary the
    /// acceptance criteria draw between a lane's own report and a person (or
    /// a lane speaking for one) editing the document out from under it.
    #[test]
    fn task_edit_refuses_a_task_that_is_neither_paused_nor_blocked() {
        let repo = fixture("edit-not-stopped");
        add(&repo, "ship", &[]);

        let from = write_doc(&repo, "mockup.txt", "new goal\n");
        let err = task_edit(
            &repo,
            &TaskEditArgs {
                task: "ship".into(),
                section: "Goal".into(),
                from,
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("queued"), "{err:#}");

        let task = queued(&repo, "ship");
        assert_eq!(
            task.section("## Goal").unwrap(),
            "Do the thing.",
            "refused, so the document is untouched"
        );
    }

    /// A `blocked` task is stopped exactly the same as a `paused` one.
    #[test]
    fn task_edit_accepts_a_blocked_task_too() {
        let repo = fixture("edit-blocked");
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let from = write_doc(&repo, "mockup.txt", "fixed\n");
        task_edit(
            &repo,
            &TaskEditArgs {
                task: "ship".into(),
                section: "Goal".into(),
                from,
            },
        )
        .unwrap();

        assert_eq!(queued(&repo, "ship").section("## Goal").unwrap(), "fixed");
    }

    /// A heading the body does not have is refused by name, rather than
    /// silently appended where `append_to_section` would have put it — an
    /// edit names a section the task's own template already put there.
    #[test]
    fn task_edit_refuses_a_heading_the_body_does_not_have() {
        let repo = fixture("edit-no-such-section");
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let from = write_doc(&repo, "mockup.txt", "text\n");
        let err = task_edit(
            &repo,
            &TaskEditArgs {
                task: "ship".into(),
                section: "Mockup".into(),
                from,
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("## Mockup"), "{err:#}");
    }
}
