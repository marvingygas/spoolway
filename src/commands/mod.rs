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
use crate::config::{Config, STATE_DIR};
use crate::fmt::{first_line, relative};
use crate::graph::{DepState, Graph};
use crate::mux::Mux;
use crate::pipeline::{Outcome, Pipeline, Pipelines, StepKind};
use crate::repo::Repo;
use crate::task::{Task, route_key, write_atomic};

/// Environment variable every lane gets, so a prompt can call `spoolway report`
/// with no arguments and still hit the right task.
pub const TASK_ENV: &str = "SPOOLWAY_TASK";

/// Refuse a command that is a person's to answer, when it is run from inside
/// a lane's own environment.
///
/// `in_lane` is read once, at the CLI boundary in `main.rs`, from whether
/// `TASK_ENV` is set — the same discipline `report`'s own `started_for`
/// argument follows, and for the same reason: the environment is process-wide
/// and a command is not, so reading it here instead would let any other
/// spoolway in the same process decide this for a caller that never asked —
/// which under `cargo test` means every fixture that queues a task, since the
/// suite itself often runs inside a real lane's own environment.
///
/// `TASK_ENV` is what tells a lane's terminal from a person's: the dispatcher
/// sets it on every lane it starts, agent or command step alike, and nothing
/// else does. A person typing into their own shell never carries it — the one
/// way they end up with it set is typing inside a lane's own pane, which is
/// exactly the case this exists to catch: `spoolway resume` run from there
/// would let a lane answer its own gate, with nobody outside it ever asked.
///
/// `what` names the verb this refusal is about, so the message reads as a
/// sentence: `"a gate is answered"`, `"a lane is answered"`, `"the queue is
/// mutated"`.
pub fn refuse_from_lane(what: &str, in_lane: bool) -> Result<()> {
    if in_lane {
        bail!(
            "this is a lane's own environment, and {what} from outside it. Ask the person \
             watching the board."
        );
    }
    Ok(())
}

mod agent;
mod config;
mod dispatch;
mod doctor;
mod group;
mod init;
mod issue;
mod lanes;
mod pending;
mod pipeline;
mod queue;
mod report;
mod routines;
mod stack;
mod task;

pub use agent::*;
pub use config::*;
pub use dispatch::*;
pub use doctor::*;
pub use group::*;
pub use init::*;
pub use issue::*;
pub use lanes::*;
pub use pipeline::*;
pub use queue::*;
pub use report::*;
pub use stack::*;
pub use task::*;

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
