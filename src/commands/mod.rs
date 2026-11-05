//! Command implementations.
//!
//! Each of these is a deterministic operation a skill or prompt can invoke
//! instead of being told how to do it in prose.
//!
//! One file per command family; this module re-exports them flat, so a
//! caller says `commands::dispatch` without caring which file it lives in.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::assets;
use crate::cli::*;
use crate::config::Config;
use crate::fmt::{first_line, relative};
use crate::graph::{DepState, Graph};
use crate::mux::Mux;
use crate::pipeline::{Outcome, Pipeline, Pipelines, Step, StepKind};
use crate::repo::Repo;
use crate::task::{Task, write_atomic};

/// Environment variable every lane gets, so a prompt can call `spoolway report`
/// with no arguments and still hit the right task.
pub const TASK_ENV: &str = "SPOOLWAY_TASK";

/// Which steps a [`LaneRefusal`] applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedOn {
    /// Every step.
    Every,
    /// Every step except the named one, where the command is allowed.
    Except(&'static str),
    /// Only the named step; it is allowed, in this form, anywhere else.
    Only(&'static str),
}

/// One command a lane is refused, written once.
///
/// This list is read twice: by the call sites that refuse, and by
/// `spoolway pipeline check`, which fails a prompt that tells its lane to run
/// one of these. A second copy of it in the lint would drift from the
/// refusals, and the drift would show up as a lane blocked twenty minutes in.
#[derive(Debug, Clone, Copy)]
pub struct LaneRefusal {
    /// The verb the message is about; see [`refuse_from_lane`].
    pub what: &'static str,
    /// The words after `spoolway` that name the command.
    pub command: &'static [&'static str],
    /// The long flag that triggers the refusal, when the bare command is allowed.
    pub flag: Option<&'static str>,
    /// The short spelling of `flag`.
    pub short: Option<char>,
    pub on: RefusedOn,
}

pub mod refusal {
    use super::{LaneRefusal, RefusedOn};

    pub const QUEUE_ADD_FROM: LaneRefusal = LaneRefusal {
        what: "the queue is mutated",
        command: &["queue", "add"],
        // Bare `queue add` prints the task skeleton, so only `--from` refuses.
        flag: Some("from"),
        short: None,
        on: RefusedOn::Every,
    };
    pub const LANE_ANSWER: LaneRefusal = LaneRefusal {
        what: "a lane is answered",
        command: &["lane"],
        flag: Some("message"),
        short: Some('m'),
        on: RefusedOn::Every,
    };
    pub const RESTART: LaneRefusal = LaneRefusal {
        what: "a step is restarted",
        command: &["restart"],
        flag: None,
        short: None,
        on: RefusedOn::Every,
    };
    /// A lane on `blocked` may resume another stopped task.
    pub const RESUME: LaneRefusal = LaneRefusal {
        what: "a gate is answered",
        command: &["resume"],
        flag: None,
        short: None,
        on: RefusedOn::Except(crate::pipeline::BLOCKED),
    };
    pub const RESUME_SKIP_HOOK: LaneRefusal = LaneRefusal {
        what: "a hook is skipped",
        command: &["resume"],
        flag: Some("skip-hook"),
        short: None,
        on: RefusedOn::Every,
    };
    /// Refused in its own words by `resume_checked`, because on `blocked` the
    /// generic gate refusal does not apply and this one is narrower.
    pub const RESUME_STAGE_ON_BLOCKED: LaneRefusal = LaneRefusal {
        what: "a task is rerouted",
        command: &["resume"],
        flag: Some("stage"),
        short: None,
        on: RefusedOn::Only(crate::pipeline::BLOCKED),
    };
}

/// Every refusal a lane meets, for the lint in `src/prompt.rs`.
pub const LANE_REFUSALS: &[&LaneRefusal] = &[
    &refusal::QUEUE_ADD_FROM,
    &refusal::LANE_ANSWER,
    &refusal::RESTART,
    &refusal::RESUME,
    &refusal::RESUME_SKIP_HOOK,
    &refusal::RESUME_STAGE_ON_BLOCKED,
];

/// Refuse a command that is a person's to answer, when it is run from inside
/// a lane's own environment.
///
/// `in_lane` is read once, at the CLI boundary in `main.rs` — the same
/// discipline `report`'s own `started_for` argument follows, and for the
/// same reason: the environment is process-wide and a command is not, so
/// reading it here instead would let any other spoolway in the same process
/// decide this for a caller that never asked — which under `cargo test`
/// means every fixture that queues a task, since the suite itself often runs
/// inside a real lane's own environment.
///
/// `queue`, `lane` and `jobs` read it from whether `TASK_ENV` is set, which
/// is what tells a lane's terminal from a person's: the dispatcher sets it on
/// every lane it starts, agent or command step alike, and nothing else does.
/// A person typing into their own shell never carries it — the one way they
/// end up with it set is typing inside a lane's own pane, which is exactly
/// the case this exists to catch.
///
/// `resume` reads `ENV_STEP` (`SPOOLWAY_STEP`) instead, and calls this only
/// when that step is not `blocked` — a lane on `blocked` may resume another
/// stopped task, bounded the ways `commands::report::resume` itself enforces,
/// so it is the one caller this function does not see at all on that one
/// step. Everywhere else, `spoolway resume` run from a lane would let it
/// answer its own gate, with nobody outside it ever asked.
///
/// `refusal.what` names the verb this refusal is about, so the message reads as a
/// sentence: `"a gate is answered"`, `"a lane is answered"`, `"the queue is
/// mutated"`.
pub fn refuse_from_lane(refusal: &LaneRefusal, in_lane: bool) -> Result<()> {
    if in_lane {
        let what = refusal.what;
        bail!(
            "this is a lane's own environment, and {what} from outside it. ask the person \
             watching the board."
        );
    }
    Ok(())
}

/// A refusal that is the caller's to correct rather than a failure:
/// `spoolway` exits 2 on it, where every other error exits 1, and prints the
/// text as it stands under an `error:` label. See `main`.
#[derive(Debug)]
pub struct UsageRefusal(pub String);

impl std::fmt::Display for UsageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageRefusal {}

mod agent;
mod config;
mod dispatch;
mod doctor;
mod group;
mod herdr;
mod hook;
pub(crate) mod init;
mod issue;
mod jobs;
mod lanes;
mod r#override;
pub(crate) mod pending;
mod pending_delete;
mod pipeline;
mod queue;
mod report;
mod routines;
mod stack;
mod task;
mod template;

pub use agent::*;
pub use config::*;
pub use dispatch::*;
pub use doctor::*;
pub use group::*;
pub use herdr::*;
pub use hook::*;
pub use init::*;
pub use issue::*;
pub use jobs::*;
pub use lanes::*;
pub use r#override::*;
pub use pipeline::*;
pub use queue::*;
pub use report::*;
pub use stack::*;
pub use task::*;
pub use template::*;

/// Refuse `--json` on a command that only has a prose form.
///
/// `--json` is global, so clap accepts it on every command. Without this, a
/// script that passes it to a prose-only command gets prose on stdout and
/// fails inside its own JSON parser, far from the cause. An error here
/// names the command and exits non-zero instead.
pub fn refuse_json(command: &str, json: bool) -> Result<()> {
    if json {
        bail!(
            "`spoolway {command}` has no JSON form, so `--json` cannot apply to it. Run it \
             without `--json` to read its text."
        );
    }
    Ok(())
}

/// Where a program resolves on PATH, if at all.
///
/// Defers to [`crate::platform::which`] rather than repeating it: that one
/// already knows an agent is `pi.cmd` on Windows and `pi` everywhere else, and
/// a doctor that looked only for the bare name would report every agent
/// missing on a machine where they are all installed. It also requires the
/// execute bit on Unix — the one caller asks whether an *agent* is installed,
/// so a `pi` that cannot be run is the same as no `pi`.
fn which(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    crate::platform::which(program, &path).map(|found| found.display().to_string())
}

#[cfg(test)]
pub mod testutil;

#[cfg(test)]
mod tests {
    /// Every `refuse_from_lane(` call passes an entry of `refusal::`, and every
    /// entry the list names is passed by some call site (the `--stage` one is
    /// the exception: `resume_checked` refuses it in its own words).
    #[test]
    fn call_sites_and_the_refusal_list_agree() {
        let sources = [
            include_str!("queue.rs"),
            include_str!("lanes.rs"),
            include_str!("report.rs"),
        ];
        let mut used = Vec::new();
        for source in sources {
            for line in source.lines() {
                let line = line.trim();
                if line.starts_with("///") || line.starts_with("//") {
                    continue;
                }
                let Some(rest) = line.strip_prefix("refuse_from_lane(") else {
                    continue;
                };
                let arg = rest.split(',').next().unwrap();
                let name = arg
                    .strip_prefix("&refusal::")
                    .unwrap_or_else(|| panic!("call site not from the list: {line}"));
                used.push(name.to_string());
            }
        }
        for refusal in super::LANE_REFUSALS {
            if refusal.command == ["resume"] && refusal.flag == Some("stage") {
                continue;
            }
            let found = used.iter().any(|name| {
                let entry = match name.as_str() {
                    "QUEUE_ADD_FROM" => &super::refusal::QUEUE_ADD_FROM,
                    "LANE_ANSWER" => &super::refusal::LANE_ANSWER,
                    "RESTART" => &super::refusal::RESTART,
                    "RESUME" => &super::refusal::RESUME,
                    "RESUME_SKIP_HOOK" => &super::refusal::RESUME_SKIP_HOOK,
                    other => panic!("unknown refusal {other}"),
                };
                entry.what == refusal.what
            });
            assert!(found, "no call site refuses {:?}", refusal.what);
        }
    }

    /// A prose-only command must refuse `--json` by name, and stay silent
    /// without it. Before the refusal existed, `--json` was accepted and
    /// prose came out on stdout.
    #[test]
    fn refuse_json_names_the_command_and_only_fires_under_the_flag() {
        assert!(super::refuse_json("group list", false).is_ok());
        let err = super::refuse_json("group list", true)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`spoolway group list` has no JSON form"),
            "{err}"
        );
        assert!(err.contains("without `--json`"), "{err}");
    }

    /// The invariant behind `init`'s prompt test — see `init.rs` — asserted
    /// where it can actually be enforced.
    ///
    /// A behavioural test only covers the callers it happens to drive, and the
    /// bug this guards against was five callers each building
    /// `prompts_dir().join(format!("{name}.md"))` for itself — the dispatcher,
    /// `pipeline check`, `doctor`, the observer, the contract printer. Any one
    /// of them can be reintroduced without failing a test that resolves paths
    /// through `path_for` directly.
    ///
    /// So this reads the source: joining onto the prompts directory is
    /// `prompt::path_for`'s job alone, because it is the only thing that knows
    /// a project may still keep its prompts in the old flat shape.
    #[test]
    fn nothing_builds_a_prompt_path_except_the_one_function_that_should() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        // The whole tree, not one directory: `src/` grew subdirectories, and a
        // file the walk missed would be a file the rule silently stopped
        // holding for.
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

        for path in sources {
            // `path_for` is where the joining belongs, and `entries` walks the
            // directory rather than naming a file inside it.
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name == "prompt.rs" {
                continue;
            }

            // Line by line would miss the shape this actually takes in real
            // code, where rustfmt puts `.prompts_dir()` and `.join(...)` on
            // separate lines. So the search runs over the source with its
            // whitespace removed, and the line number is recovered from the
            // offset afterwards.
            let text = std::fs::read_to_string(&path).expect("reading a source file");
            let mut flat = String::with_capacity(text.len());
            let mut lines = Vec::with_capacity(text.len());
            for (number, line) in text.lines().enumerate() {
                // Prose about the rule is not a breach of it — this very test
                // quotes the pattern it forbids.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for character in line.chars().filter(|c| !c.is_whitespace()) {
                    flat.push(character);
                    lines.push(number + 1);
                }
            }

            // Assembled rather than written out, so this line is not itself the
            // first thing the search finds. The leading `.` is what keeps this
            // from also flagging `repo.system_prompts_dir().join(...)` —
            // composing the *composed system prompt's* own path, a different
            // directory this rule has nothing to say about — since that call
            // has no `.` immediately before `prompts_dir(`, only `_`.
            let needle = format!(".prompts_dir(){}", ".join(");
            for (at, _) in flat.match_indices(&needle) {
                offenders.push(format!("{name}:{}", lines[at]));
            }
        }

        assert!(
            offenders.is_empty(),
            "build prompt paths with `prompt::path_for`, which reads both the \
             directory and the legacy flat shape:\n  {}",
            offenders.join("\n  ")
        );
    }
}
