//! Every command a lane is refused, written once.
//!
//! Two readers depend on this list. The call sites that refuse take their
//! entry from it, and `spoolway pipeline check` fails a prompt that tells its
//! lane to run one. A second copy in the lint would drift from the refusals,
//! and the drift would show up as a lane blocked twenty minutes in.
//!
//! A [`LaneRefusal`] has private fields, so only this file can make one. A
//! refusal added anywhere else has to be added here, and the test below fails
//! for an entry that no call site reads.

/// Which steps a [`LaneRefusal`] applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedOn {
    /// Every step.
    Every,
    /// Every step except the named one, where the command is allowed.
    Except(&'static str),
    /// Only the named step; this entry does not apply on any other step.
    Only(&'static str),
}

impl RefusedOn {
    /// Whether the refusal applies to a lane on `step`.
    pub fn applies_on(self, step: &str) -> bool {
        match self {
            RefusedOn::Every => true,
            RefusedOn::Except(allowed) => step != allowed,
            RefusedOn::Only(only) => step == only,
        }
    }
}

/// One command a lane is refused.
#[derive(Debug, Clone, Copy)]
pub struct LaneRefusal {
    /// The constant's own name, so the test can find the call sites that read it.
    #[cfg(test)]
    id: &'static str,
    /// What is done that a lane may not do, as a clause that reads after
    /// "refuses in a lane:" and before "from outside it".
    what: &'static str,
    /// The words after `spoolway` that name the command.
    command: &'static [&'static str],
    /// The long flag that triggers the refusal, when the bare command is allowed.
    flag: Option<&'static str>,
    /// The short spelling of `flag`.
    short: Option<char>,
    /// The steps it applies to.
    on: RefusedOn,
}

impl LaneRefusal {
    pub fn what(&self) -> &'static str {
        self.what
    }
    pub fn command(&self) -> &'static [&'static str] {
        self.command
    }
    pub fn flag(&self) -> Option<&'static str> {
        self.flag
    }
    pub fn short(&self) -> Option<char> {
        self.short
    }
    /// Whether the refusal applies to a lane on `step`.
    pub fn applies_on(&self, step: &str) -> bool {
        self.on.applies_on(step)
    }
}

/// Builds one entry. The name is kept only for the test that looks for its
/// call sites, so the binary does not carry it.
macro_rules! entry {
    ($id:literal, $what:expr, $command:expr, $flag:expr, $short:expr, $on:expr $(,)?) => {
        LaneRefusal {
            #[cfg(test)]
            id: $id,
            what: $what,
            command: $command,
            flag: $flag,
            short: $short,
            on: $on,
        }
    };
}

/// The overrides layer and the private copies live outside the worktree, so a
/// lane's write there reaches the person's own setup. See
/// [`crate::repo::Repo::refuse_in_lane`].
const SETUP: &str = "the project's setup is changed";

pub const QUEUE_ADD_FROM: LaneRefusal = entry!(
    "QUEUE_ADD_FROM",
    "the queue is mutated",
    &["queue", "add"],
    // Bare `queue add` prints the task skeleton, so only `--from` refuses.
    Some("from"),
    None,
    RefusedOn::Every,
);
pub const LANE_ANSWER: LaneRefusal = entry!(
    "LANE_ANSWER",
    "a lane is answered",
    &["lane"],
    Some("message"),
    Some('m'),
    RefusedOn::Every,
);
pub const RESTART: LaneRefusal = entry!(
    "RESTART",
    "a step is restarted",
    &["restart"],
    None,
    None,
    RefusedOn::Every,
);
/// A lane on `blocked` may resume another stopped task.
pub const RESUME: LaneRefusal = entry!(
    "RESUME",
    "a gate is answered",
    &["resume"],
    None,
    None,
    RefusedOn::Except(crate::pipeline::BLOCKED),
);
pub const RESUME_SKIP_HOOK: LaneRefusal = entry!(
    "RESUME_SKIP_HOOK",
    "a hook is skipped",
    &["resume"],
    Some("skip-hook"),
    None,
    RefusedOn::Every,
);
/// Rerouting a task is a decision about what the work is for. On `blocked`
/// the plain refusal does not apply, so this narrower one does.
pub const RESUME_STAGE_ON_BLOCKED: LaneRefusal = entry!(
    "RESUME_STAGE_ON_BLOCKED",
    "a task is rerouted",
    &["resume"],
    Some("stage"),
    None,
    RefusedOn::Only(crate::pipeline::BLOCKED),
);
pub const PIPELINE_OVERRIDE: LaneRefusal = entry!(
    "PIPELINE_OVERRIDE",
    SETUP,
    &["pipeline", "override"],
    None,
    None,
    RefusedOn::Every,
);
pub const PIPELINE_COPY: LaneRefusal = entry!(
    "PIPELINE_COPY",
    SETUP,
    &["pipeline", "copy"],
    None,
    None,
    RefusedOn::Every,
);
pub const PIPELINE_RENAME: LaneRefusal = entry!(
    "PIPELINE_RENAME",
    SETUP,
    &["pipeline", "rename"],
    None,
    None,
    RefusedOn::Every,
);
pub const PROMPT_COPY: LaneRefusal = entry!(
    "PROMPT_COPY",
    SETUP,
    &["prompt", "copy"],
    None,
    None,
    RefusedOn::Every,
);
pub const PROMPT_OVERRIDE: LaneRefusal = entry!(
    "PROMPT_OVERRIDE",
    SETUP,
    &["prompt", "override"],
    None,
    None,
    RefusedOn::Every,
);
/// Refused with or without `--set`: bare `config override` opens an editor on
/// the layer.
pub const CONFIG_OVERRIDE: LaneRefusal = entry!(
    "CONFIG_OVERRIDE",
    SETUP,
    &["config", "override"],
    None,
    None,
    RefusedOn::Every,
);
/// The three promote commands and `config set` write the tracked setup, which
/// [`crate::repo::Repo::tracked_write_root`] refuses in a lane.
pub const CONFIG_SET: LaneRefusal = entry!(
    "CONFIG_SET",
    SETUP,
    &["config", "set"],
    None,
    None,
    RefusedOn::Every,
);
pub const OVERRIDE_PROMOTE: LaneRefusal = entry!(
    "OVERRIDE_PROMOTE",
    SETUP,
    &["override", "promote"],
    None,
    None,
    RefusedOn::Every,
);
pub const PIPELINE_PROMOTE: LaneRefusal = entry!(
    "PIPELINE_PROMOTE",
    SETUP,
    &["pipeline", "promote"],
    None,
    None,
    RefusedOn::Every,
);
pub const OVERRIDE_DROP: LaneRefusal = entry!(
    "OVERRIDE_DROP",
    SETUP,
    &["override", "drop"],
    None,
    None,
    RefusedOn::Every,
);

/// Every refusal a lane meets, for the lint in `src/prompt.rs`.
pub const LANE_REFUSALS: &[&LaneRefusal] = &[
    &QUEUE_ADD_FROM,
    &LANE_ANSWER,
    &RESTART,
    &RESUME,
    &RESUME_SKIP_HOOK,
    &RESUME_STAGE_ON_BLOCKED,
    &PIPELINE_OVERRIDE,
    &PIPELINE_COPY,
    &PIPELINE_RENAME,
    &PROMPT_COPY,
    &PROMPT_OVERRIDE,
    &CONFIG_OVERRIDE,
    &OVERRIDE_DROP,
    &CONFIG_SET,
    &OVERRIDE_PROMOTE,
    &PIPELINE_PROMOTE,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                out.push((path, text));
            }
        }
    }

    /// An entry nobody reads is a refusal the lint judges that the code does
    /// not make, and a refusal made without an entry is one the lint cannot
    /// see. Fields are private, so the second cannot be built outside this
    /// file; this covers the first.
    #[test]
    fn every_entry_is_read_by_a_call_site() {
        let mut files = Vec::new();
        sources(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        for refusal in LANE_REFUSALS {
            let needle = format!("refusal::{}", refusal.id);
            let read = files
                .iter()
                .filter(|(path, _)| !path.ends_with("commands/refusal.rs"))
                .any(|(_, text)| {
                    text.match_indices(&needle).any(|(at, _)| {
                        !text[at + needle.len()..]
                            .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                    })
                });
            assert!(read, "no call site reads {needle}");
        }
    }

    #[test]
    fn every_entry_is_listed_once() {
        let mut ids: Vec<_> = LANE_REFUSALS.iter().map(|r| r.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), LANE_REFUSALS.len());
    }
}
