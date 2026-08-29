//! The task-document contract, and checking a document against it before it
//! is queued.
//!
//! `spoolway task contract` is the whole interface a producer needs. Printed
//! bare, it is every key a document may set, every key it may not, which
//! pipeline is this project's default, each pipeline's id budget, the steps
//! `gate_at` accepts, the body skeleton itself, and — under `output` — the
//! directory a finished document is written to, what to name it there, and
//! the two commands that check it and send it. One call, so a producer never
//! has to be told anything a person read somewhere else: a model handed this
//! JSON and a goal has everything it needs to leave a queueable task on
//! disk. Run with `--from`, it is the exact validation `queue add --from`
//! runs, with nothing written at the end of it.
//!
//! Both modes are read straight off [`super::queue::RESERVED_KEYS`],
//! [`super::queue::longest_agent_step`], [`super::queue::gather_documents`]
//! and [`super::queue::validate_batch`] — the functions that actually
//! enforce the contract — so this can never say something the enforcement
//! does not, or the other way round.

use super::*;

/// Keys a document must set — refused by `parse_submission` when blank or
/// absent.
const REQUIRED_KEYS: &[&str] = &["id", "title", "group"];

/// Keys a document may set, and spoolway keeps exactly what it wrote.
const OPTIONAL_KEYS: &[&str] = &[
    "source",
    "plan",
    "touches",
    "depends_on",
    "parallel",
    "pipeline",
    "gate_at",
    "epic",
    "ticket",
];

/// The keys spoolway's own dispatcher machinery overwrites unconditionally
/// once a document reaches `parse_submission`, whatever value the document
/// gave them — see the block of `front.<field> = ...` assignments there.
/// Distinct from [`super::queue::RESERVED_KEYS`]: setting one of *these* is
/// not refused, it is simply thrown away, because a document cannot know the
/// run id, the worktree path or the launch counters before any of them
/// exist.
const IGNORED_KEYS: &[&str] = &[
    "base",
    "borrowed",
    "last_report",
    "blocked_from",
    "parked_from",
    "resume",
    "branch",
    "patch",
    "skip",
    "replay_of",
    "worktree_path",
    "workspace_id",
    "pane_id",
    "tab_id",
    "paused_at",
    "launched_at",
    "usage_limit_hold",
    "prompts",
    "rounds",
    "arrived_from",
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
         suffix and half a lane name, so it must fit the id budget of the pipeline \
         this task runs on.",
    ),
    (
        "title",
        "A short, present-tense sentence naming what this task does — the subject \
         of the eventual commit and pull request.",
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
        "Which pipeline to run this task on — leave unset to use the default \
         named at the top of this contract.",
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
    ]
}

/// The longest id a task on this pipeline may have — [`crate::mux::lane_name`]'s
/// own budget, worked out the same way [`crate::mux::check_task_id`] checks
/// it rather than a copy of its arithmetic: [`crate::mux::LANE_NAME_MAX`]
/// minus what an empty id's own lane name already costs, which is exactly the
/// separator [`crate::mux::tab_label`] puts between the two halves.
fn id_budget(longest_step: &str) -> usize {
    crate::mux::LANE_NAME_MAX.saturating_sub(crate::mux::tab_label("", longest_step).len())
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

/// One pipeline's own slice of the contract: the constraint an id has to fit
/// under, the steps a `gate_at` may name, and the body a task on it is
/// written from.
#[derive(Debug, serde::Serialize)]
struct PipelineContract {
    longest_agent_step: String,
    id_budget: usize,
    gate_at: Vec<String>,
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

/// The whole task-document contract, printed as JSON by bare `spoolway task
/// contract`.
#[derive(Debug, serde::Serialize)]
struct Contract {
    default: String,
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
            (
                name.clone(),
                PipelineContract {
                    longest_agent_step: longest.to_string(),
                    id_budget: id_budget(longest),
                    gate_at: pipeline.steps.iter().map(|s| s.id.clone()).collect(),
                    body,
                },
            )
        })
        .collect();

    let pending = repo.pending_dir();
    let pending = pending.display().to_string();

    Contract {
        default: pipelines.default.clone(),
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

        let pipeline_name = task.front.pipeline.as_deref().unwrap_or(&pipelines.default);
        let pipeline = pipelines.get(pipeline_name)?;
        let budget = id_budget(super::queue::longest_agent_step(pipeline));
        let spare = budget.saturating_sub(task.id().len());

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
            (
                "id",
                format!(
                    "`{}` fits a lane name at `{pipeline_name}`'s longest step, with \
                     {spare} characters to spare",
                    task.id()
                ),
            ),
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
    cwd: &std::path::Path,
) -> Result<()> {
    if args.from.is_empty() {
        return print_contract(repo, pipelines);
    }

    let base = crate::repo::branch_at(cwd)?;
    let documents = super::queue::gather_documents(&args.from)?;
    let tasks = super::queue::validate_batch(repo, pipelines, &base, &documents)?;
    print_check_report(&tasks, repo, pipelines)
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
    fn document(id: &str, extra: &str, body: &str) -> String {
        format!("---\nid: {id}\ntitle: {id}, done\n{extra}---\n{body}")
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
        }
    }

    fn from_args(paths: &[&str]) -> QueueAddArgs {
        QueueAddArgs {
            from: paths.iter().map(|p| p.to_string()).collect(),
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
        assert_eq!(value["default"], Pipelines::builtin().default);
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
            value["pipelines"]["default"]["id_budget"].is_u64(),
            "{value}"
        );
        assert!(
            value["pipelines"]["default"]["gate_at"].is_array(),
            "{value}"
        );
        assert!(value["pipelines"]["default"]["body"].is_string(), "{value}");
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

    /// The id budget in the printed contract is the same one
    /// `check_task_id` actually enforces — a document trusting the printed
    /// number and one over it is refused, right at that boundary.
    #[test]
    fn the_printed_id_budget_matches_check_task_id() {
        let pipeline = Pipelines::builtin();
        let longest =
            super::super::queue::longest_agent_step(pipeline.get(&pipeline.default).unwrap());
        let budget = id_budget(longest);

        let fits = "a".repeat(budget);
        assert!(crate::mux::check_task_id(&fits, longest).is_ok(), "{fits}");
        let over = "a".repeat(budget + 1);
        assert!(crate::mux::check_task_id(&over, longest).is_err(), "{over}");
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
}
