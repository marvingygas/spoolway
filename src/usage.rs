//! What each lane actually spent.
//!
//! spoolway never sees a token: it starts `pi` and `claude` in panes and reads
//! nothing they say to a model. So usage is not instrumented, it is *collected*
//! — both agents already write a JSONL transcript with per-turn token counts,
//! and spoolway's only real job is knowing which transcript belongs to which task
//! and step.
//!
//! That correlation is the whole design, and it is always a lookup by a name
//! spoolway chose rather than a guess from a working directory and a
//! modification time. Which part of the path carries that name depends on what
//! the CLI will accept. Most take a session id, so the dispatcher mints one per
//! lane, passes it through the `args` template as `{session_id}`, and finds the
//! transcript by its filename. One — codex — mints its own and refuses any
//! other, so what spoolway names is the *directory* instead: see
//! [`FileShape::OwnHome`] and [`crate::agent::Accounting::home_env`].
//!
//! The result is one line per finished lane in the project's own
//! `usage.jsonl` — see [`crate::repo::Repo::usage_file`] — appended
//! and never rewritten. Append-only means there is no state to reconcile and
//! nothing a crash mid-pass can corrupt; it also means the record outlives the
//! task file, which `cleanup` archives.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::platform::home_dir;
use crate::repo::Repo;

/// Runtime state, next to `lanes.json`. Untracked, like the queue.
pub const LEDGER_FILE: &str = "usage.jsonl";

/// Tokens for one lane, by kind.
///
/// The priced classes are disjoint: a provider that reports cached input
/// separately excludes it from `input`, so summing them is right and does not
/// double count. `reasoning` is *not* in [`Tokens::total`] — both agents count
/// thinking inside `output`, and adding it again would inflate every figure.
///
/// Cache writes are split by the cache's lifetime because they are two prices,
/// not one: a five-minute write costs 1.25× the input rate and an hour costs
/// 2×. Claude Code writes an hour cache by default, so charging every write at
/// the five-minute rate understates a real transcript by around a third.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Written to a five-minute cache.
    ///
    /// Ledger lines from before the split carried one undifferentiated
    /// `cache_write`; the alias folds those into this field, which is the
    /// cheaper of the two — an old line is therefore a lower bound rather than
    /// a number invented for it.
    #[serde(alias = "cache_write")]
    pub cache_write_5m: u64,
    /// Written to a one-hour cache.
    pub cache_write_1h: u64,
    /// Reported where the provider breaks it out. Informational: already
    /// counted in `output`.
    pub reasoning: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write()
    }

    /// Every cache write, whatever its lifetime. For display and totals only —
    /// pricing must use the two fields separately, because they are two rates.
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.reasoning += other.reasoning;
    }

    /// What is in `self` and not yet in `already`.
    ///
    /// Saturating, because the only way a total goes backwards is a transcript
    /// that was truncated or rolled over, and the honest answer to "how much
    /// more?" is then zero rather than a negative that would credit the ledger.
    pub fn since(&self, already: &Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(already.input),
            output: self.output.saturating_sub(already.output),
            cache_read: self.cache_read.saturating_sub(already.cache_read),
            cache_write_5m: self.cache_write_5m.saturating_sub(already.cache_write_5m),
            cache_write_1h: self.cache_write_1h.saturating_sub(already.cache_write_1h),
            reasoning: self.reasoning.saturating_sub(already.reasoning),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// One finished lane, as written to the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ts: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub step: String,
    pub pipeline: String,
    /// Agent *profile* name from config — `pi`, `claude` — not the binary.
    pub agent: String,
    /// Agent kind: the CLI that ran, and so which transcript format was read.
    pub kind: String,
    pub model: String,
    pub session: String,
    #[serde(default)]
    pub round: u32,
    /// Wall-clock seconds the lane was open. Not model time: a gated lane
    /// waiting on a person is mostly this.
    #[serde(default)]
    pub wall_s: i64,
    /// Assistant turns in the transcript.
    #[serde(default)]
    pub turns: u32,
    pub tokens: Tokens,
    /// Absent when nothing could price this model — never estimated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,

    /// The largest single-turn context reading the transcript reached, over
    /// every deduped turn rather than just the last — see [`Harvest::ctx_peak`],
    /// which this is copied from. Absent on a line written before this field
    /// existed, and on a lane whose transcript could not be read at all —
    /// never estimated, the same rule [`Entry::cost_usd`] follows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_peak: Option<u64>,

    /// Fingerprint of the tracked configuration this lane ran under — see
    /// [`crate::version`]. Absent on lines written before versions were
    /// recorded, which read as `unversioned` rather than being dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Short commit that last touched that configuration, `+dirty` where the
    /// working tree had edits git never saw. The half of a version that can be
    /// turned back into a diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// What this lane reported: `pass`, `fail` or `block`.
    ///
    /// Absent means the lane never reported one — it was killed, or it ended
    /// its turn silently. That is a real state and worth telling apart from a
    /// failure, so it is left absent rather than filled in with a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,

    /// The run this lane was banked for: minted once, with the task's
    /// worktree, and copied onto every lane banked for that task from then on.
    ///
    /// Absent on every line written before runs existed — [`crate::eval`]
    /// falls back to a key derived from the task and its earliest round, so a
    /// historical pass is still nameable rather than left out of the table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// The trial this task is one arm of, banked verbatim from
    /// `task.front.trial` — see [`crate::task::Frontmatter::trial`]. Absent on
    /// every ordinary task, which is what lets `spoolway eval --runs --trial
    /// <id>` find only the arms and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<String>,

    /// Which skill was running when this was spent, for a line banked from an
    /// interactive session rather than a lane: `plan`, `queue`, `prompt`, or
    /// [`INTERACTIVE_SKILL`] for the stretches with no skill in the chair.
    ///
    /// Absent on every lane line, which is what tells the two apart — see
    /// [`Entry::skill_label`], which also reads the planning lines written
    /// before this field existed. Deliberately not folded into `step`: a
    /// pipeline is free to declare a step called `plan`, and the two must
    /// never merge into one row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,

    /// Which project this lane ran in. Never written to disk — a ledger lives
    /// inside its project, so storing the answer in every line would be a
    /// thousand copies of the file's own path. Filled in at load time, and only
    /// interesting once more than one ledger is being read at once.
    #[serde(skip)]
    pub project: String,
}

/// The agent name every interactive line carries, lane or no lane.
pub const INTERACTIVE_AGENT: &str = "interactive";

/// What the one stretch of an interactive session with no command in the
/// chair yet is labelled — turns before the transcript's first marker. Every
/// slash command after that banks under its own name, `skill_of`'s.
pub const INTERACTIVE_SKILL: &str = "interactive";

/// The planning skill's real name, and what every older shape of a planning
/// line is read as.
///
/// Two older shapes reach this. `queue add --plan` banked a whole planning
/// session under a step called `plan`, before [`Entry::skill`] existed at
/// all, so those lines carry no skill; and [`skill_of`] used to strip a
/// `spoolway-` prefix, so lines it wrote carry the bare `plan`. The ledger is
/// append-only, so neither is rewritten — both are *read* as
/// `spoolway-plan`, which is what keeps an existing ledger totalling to the
/// same money under the name the config now uses.
const PLANNING_SKILL: &str = "spoolway-plan";

/// The bare name the stripping [`skill_of`] used to write, and the step name
/// `queue add --plan` banked under. Never written again; only read.
const LEGACY_PLANNING_SKILL: &str = "plan";

/// The model name Claude Code writes on a message it generated locally, not
/// one a model answered: a spend-limit notice, an API error, an interrupt.
/// The turn's `usage` block is all zeroes, so counting it would both bank the
/// lane under a model nothing prices and report the conversation's size as
/// zero even when a real turn just before it was not small at all. Neither
/// [`read_transcript`] nor [`last_turn_at`] lets a turn under this name
/// overwrite the model, the token totals or the context reading they build.
const SYNTHETIC_MODEL: &str = "<synthetic>";

impl Entry {
    /// Which skill this line is spend for, or `None` for a lane.
    ///
    /// The one place the legacy shape is understood, so that everything
    /// grouping, sweeping or totalling over the ledger asks the same question
    /// and gets the same answer.
    pub fn skill_label(&self) -> Option<&str> {
        match self.skill.as_deref() {
            // A line the stripping `skill_of` wrote. `plan` was never a
            // command anybody typed, so reading it back under the planning
            // skill's real name cannot shadow a project's own skill.
            Some(LEGACY_PLANNING_SKILL) => Some(PLANNING_SKILL),
            Some(skill) => Some(skill),
            None => (self.agent == INTERACTIVE_AGENT).then_some(PLANNING_SKILL),
        }
    }

    /// Whether this line is a skill session's rather than a lane's.
    pub fn is_skill(&self) -> bool {
        self.skill_label().is_some()
    }
}

/// A model's window and its per-1M-token prices, matching one glob over its
/// name.
///
/// Named after the model rather than the provider because that is what a
/// transcript reports, and what a step's `model:` names. Holds both facts
/// rather than one apiece because both are true of the model itself, not of
/// a launch profile — `agents.*.context_window` and `[pricing]` used to be
/// two places disagreeing about that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelPrice {
    /// What one session gets to work in, in tokens. Already final: spoolway
    /// divides it by nothing.
    ///
    /// Mostly planning: the planning skill uses it to judge whether a task is
    /// small enough for this model to finish in one sitting, and it does not
    /// truncate, chunk, or cap anything a lane does. `dispatch::start_one` is
    /// the one runtime reader — a `session:` step's declared bound is a
    /// percentage of this, so a step naming a model with no window here never
    /// resumes, only ever opens fresh. Zero means unset; a hosted model's
    /// planner falls back to what it publishes.
    #[serde(skip_serializing_if = "unset_usize")]
    pub context_window: usize,
    /// USD per 1M input tokens.
    #[serde(skip_serializing_if = "unset_rate")]
    pub input: f64,
    /// USD per 1M output tokens.
    #[serde(skip_serializing_if = "unset_rate")]
    pub output: f64,
    /// USD per 1M tokens read from the prompt cache. Typically 0.1× input.
    #[serde(skip_serializing_if = "unset_rate")]
    pub cache_read: f64,
    /// USD per 1M tokens written to a five-minute cache. Typically 1.25× input.
    ///
    /// Aliased so a config written before the split keeps working: what used to
    /// be one `cache_write` rate is the five-minute one.
    #[serde(alias = "cache_write", skip_serializing_if = "unset_rate")]
    pub cache_write_5m: f64,
    /// USD per 1M tokens written to a one-hour cache. Typically 2× input.
    ///
    /// Left at zero it falls back to the five-minute rate rather than pricing
    /// an hour cache at nothing — a config that predates the split under-reports
    /// rather than silently dropping a whole class of spend.
    #[serde(skip_serializing_if = "unset_rate")]
    pub cache_write_1h: f64,

    /// How long a carried session may sit before `dispatch::carried_session`
    /// refuses to resume it — measured against this model's own store,
    /// `usage::touched_at`.
    ///
    /// **Why this is a model's fact and not an agent kind's.** A cache belongs
    /// to whoever serves the model, and pi and codex are *harnesses*
    /// — the same binary talks to Anthropic, to OpenAI or to a llama.cpp
    /// socket on the next port, and no fact about the harness could ever have
    /// answered which. What holds a cache is the provider behind the model, so
    /// the model is where its lifetime is declared — beside the prices, which
    /// are the same kind of fact and are already matched by the same glob.
    ///
    /// `None` means nobody has said, and a carried session under this model
    /// is never refused for its age — the honest answer, not a guess that it
    /// has gone cold. Renamed from `cache_ttl`: the alias keeps an existing
    /// config parsing, and this is consulted on every carried session now
    /// rather than only on a turn that happened to touch a cache, which is
    /// what the old name overstated.
    #[serde(
        alias = "cache_ttl",
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::config::human_duration::optional"
    )]
    pub session_reuse_idle: Option<std::time::Duration>,

    /// How many lanes may run this model at once, in place of its profile's
    /// `agents.<profile>.concurrency` when set.
    ///
    /// A profile's concurrency caps a *binary* — how many `pi` processes this
    /// machine will run at a time — but a local server's own limit is per
    /// *model*: swap `llama.cpp`'s loaded weights and the number of parallel
    /// slots changes with it, whatever a profile still says. `0` means unset,
    /// the same sentinel [`Self::context_window`] uses, so the profile's
    /// figure still answers for a model nobody has sized this way.
    #[serde(skip_serializing_if = "unset_slots")]
    pub slots: u32,

    /// This model never runs alongside a different model also carrying
    /// `exclusive = true`, however many slots either has and whatever the
    /// step's own `slot:` says.
    ///
    /// For a local server that can only ever have one set of weights loaded
    /// at a time: two `exclusive` models are a statement that swapping one in
    /// evicts the other, not a request for more concurrency. A step whose
    /// `slot:` is `false` still waits behind a live `exclusive` model — the
    /// exclusion is about what the server can hold, which a step opting out
    /// of the profile's slot budget does not change.
    #[serde(skip_serializing_if = "not_exclusive")]
    pub exclusive: bool,
}

/// A zero in `[models]` is *unset*, and unset is written by leaving the key
/// out. Every rate in this table is zero for a local model, and eight lines
/// saying a free model is free is noise around the two fields that carry the
/// answer. See [`ModelPrice::probe`] for the one thing this costs.
fn unset_usize(n: &usize) -> bool {
    *n == 0
}

fn unset_rate(rate: &f64) -> bool {
    *rate == 0.0
}

fn unset_slots(slots: &u32) -> bool {
    *slots == 0
}

fn not_exclusive(exclusive: &bool) -> bool {
    !*exclusive
}

impl ModelPrice {
    /// A value with every field present when serialised — nothing zero,
    /// nothing `None`.
    ///
    /// `ModelPrice::default()` used to serve as the shape of this table: every
    /// field was written, so a rendered default named them all. It does not
    /// any more, because a default is all zeros and a zero is now omitted, so
    /// a rendered default is an empty table. Anything that needs the field
    /// *names* rather than a model's actual values asks for this instead —
    /// `confkv` uses it to tell a real field from a typo, and to know what
    /// type a field it is about to create should hold. The values are
    /// deliberately meaningless; only the keys and their types are read.
    pub fn probe() -> Self {
        Self {
            context_window: 1,
            input: 1.0,
            output: 1.0,
            cache_read: 1.0,
            cache_write_5m: 1.0,
            cache_write_1h: 1.0,
            session_reuse_idle: Some(std::time::Duration::from_secs(1)),
            slots: 1,
            exclusive: true,
        }
    }

    fn apply(&self, tokens: &Tokens) -> f64 {
        let per = |n: u64, rate: f64| (n as f64) * rate / 1_000_000.0;
        let hourly = match self.cache_write_1h {
            0.0 => self.cache_write_5m,
            rate => rate,
        };
        per(tokens.input, self.input)
            + per(tokens.output, self.output)
            + per(tokens.cache_read, self.cache_read)
            + per(tokens.cache_write_5m, self.cache_write_5m)
            + per(tokens.cache_write_1h, hourly)
    }
}

/// Price `tokens` for `model`, or `None` if it resolves to nothing.
///
/// Resolves this project's own `[models]` table first, by glob, then falls
/// back to `crate::models`' vendored built-in table by exact name — see
/// [`crate::models::resolve`], which this defers to. Returning `None` rather
/// than zero is deliberate: a model in neither has an unknown cost, not a free
/// one, and `spoolway eval --by` says which models those are instead of quietly
/// under-reporting a total.
pub fn price(prices: &BTreeMap<String, ModelPrice>, model: &str, tokens: &Tokens) -> Option<f64> {
    crate::models::resolve(prices, model)
        .price
        .map(|entry| entry.apply(tokens))
}

/// The most specific pattern matching `model`, where specificity is how much of
/// the pattern is literal. `claude-opus-5` beats `claude-*` for an exact model.
///
/// Crate-visible so `crate::models::resolve` can check this project's own
/// `[models]` table before falling back to the built-in one.
pub(crate) fn best_match<'a>(
    prices: &'a BTreeMap<String, ModelPrice>,
    model: &str,
) -> Option<&'a ModelPrice> {
    prices
        .iter()
        .filter(|(pattern, _)| glob_match(pattern, model))
        .max_by_key(|(pattern, _)| pattern.chars().filter(|c| *c != '*').count())
        .map(|(_, entry)| entry)
}

/// `*` matches any run of characters; everything else is literal. Enough for
/// model names, and small enough to read. Crate-visible because the known
/// context windows in `config` match model names the same way prices do.
pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return pattern == value;
    };
    if !value.starts_with(first) {
        return false;
    }
    let mut rest = &value[first.len()..];
    let mut last: Option<&str> = None;
    for part in parts {
        last = Some(part);
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    // A pattern not ending in `*` has to consume the rest of the value.
    match last {
        // No `*` at all: the literal had to be the whole thing.
        None => rest.is_empty(),
        Some("") => true,
        Some(_) => rest.is_empty(),
    }
}

/// How the transcript for a session spoolway pinned is found again.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileShape {
    /// `<id>.jsonl`.
    Exact,
    /// `<something>_<id>.jsonl` — a timestamp prefix spoolway does not predict.
    AfterUnderscore,
    /// The id is not in the filename at all, because it is not the agent's:
    /// this kind mints its own and refuses any other. What carries spoolway's
    /// id is the *directory*, a home of this session's own — so the transcript
    /// is whichever one is in it, whatever the agent chose to call it.
    ///
    /// Only for a kind whose row also sets [`crate::agent::Accounting::home_env`],
    /// since the home has to actually be handed to the binary for any of this
    /// to be true.
    ///
    /// **Two homes, and the id says which.** All of the above is a *lane's*
    /// session, whose home spoolway made before starting it. An interactive
    /// session has no such home: it was already running when spoolway was
    /// invoked inside it, and it wrote its transcript into the agent's own
    /// home — see [`crate::agent::own_home`]. So the lookup asks first whether
    /// spoolway ever made a home under this id. If it did, the id is one
    /// spoolway minted and the transcript is whatever is inside. If it did
    /// not, the id is the agent's own, arriving from
    /// [`crate::agent::Accounting::session_env`], and the search moves to the
    /// agent's home — where the tree holds every session it ever wrote, so the
    /// id has to be matched in the filename after all.
    OwnHome {
        /// Where under that home the agent puts transcripts, and *only*
        /// transcripts.
        ///
        /// Not a formality. An agent's home is its own scratch space, and codex
        /// keeps unrelated `.jsonl` under `.tmp/plugins` — a plugin fixture
        /// that, taken as the newest file in the tree, would be read back as
        /// the lane's transcript and yield a silent zero. So the walk is rooted
        /// at the one subtree that holds the real thing rather than at the home.
        under: &'static str,
    },
}

/// Where a session's records live, and how they are enumerated.
///
/// Separate from [`Format`], which says how one record *reads*, because the two
/// vary independently. An earlier row proved it: a per-turn record as
/// parseable as anybody's, whose only unusual property was that it lived as a
/// row in a database rather than a line in a file — a fact about the
/// *container*, which is what this enum exists to isolate from the shape of
/// the record itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Store {
    /// One JSONL transcript per session, found by [`FileShape`] and read a
    /// line at a time.
    Transcript(FileShape),
}

/// The shape of one assistant record in a transcript.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `{"message":{"role":"assistant","usage":{"input":..,"cacheRead":..}}}`,
    /// with the agent's own cost already computed.
    Pi,
    /// `{"type":"assistant","message":{"usage":{"input_tokens":..}}}` — the
    /// Anthropic API's own usage shape, as Claude Code records it. Named for
    /// the shape rather than the CLI, since it is the API's, not Claude Code's.
    AnthropicApi,
    /// `{"type":"event_msg","payload":{"type":"token_count","info":{
    /// "last_token_usage":{..},"total_token_usage":{..}}}}` — codex's rollout,
    /// which reports each turn twice over: once on its own and once as the
    /// conversation's running total.
    Codex,
}

/// The one record a kind's transcript writes when a person aborts the turn in
/// progress by pressing Escape — captured verbatim off a real transcript for
/// each kind that has an established one. Read by [`last_turn_aborted`],
/// which only ever asks about the transcript's *last* record: an abort
/// buried earlier in a resumed session already has a real turn written over
/// it, and that is what answers the question now, not the interrupt.
///
/// Three different shapes, so this is a matcher per kind rather than one
/// shared string — see [`crate::agent::Adapter::abort_marker`], the row this
/// names, and the plan this task comes from,
/// `hand-interrupt-parks-the-lane`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AbortMarker {
    /// claude: a `user` record whose text is exactly
    /// `[Request interrupted by user]` —
    /// `{"type":"user","message":{"role":"user","content":
    /// [{"type":"text","text":"[Request interrupted by user]"}]}}`. Matched
    /// whether `content` arrives as that single-block array or as the plain
    /// string an ordinary user turn in the same transcript writes it as.
    ClaudeInterrupted,
    /// codex: a `user` item whose `input_text` opens with `<turn_aborted>` —
    /// `{"role":"user","content":[{"type":"input_text","text":
    /// "<turn_aborted>\nThe user interrupted the previous turn on
    /// purpose…"}]}`, wrapped the way every codex rollout record is, under a
    /// `response_item`'s `payload`. Matched by prefix rather than the whole
    /// text, since the rest of the message is codex's own wording about what
    /// it tore down and not part of the marker.
    CodexTurnAborted,
    /// pi: an `assistant` record carrying `"stopReason":"aborted"` —
    /// `{"type":"message","message":{"role":"assistant","content":[],
    /// "stopReason":"aborted","errorMessage":"Operation aborted"}}` — a
    /// field, not a string to search for.
    PiAborted,
}

impl AbortMarker {
    /// Whether one transcript record is this kind's abort marker.
    fn matches(self, value: &serde_json::Value) -> bool {
        match self {
            AbortMarker::ClaudeInterrupted => {
                if value.get("type").and_then(|t| t.as_str()) != Some("user") {
                    return false;
                }
                match value.get("message").and_then(|m| m.get("content")) {
                    Some(serde_json::Value::String(text)) => {
                        text == "[Request interrupted by user]"
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        blocks.len() == 1
                            && blocks[0].get("text").and_then(|t| t.as_str())
                                == Some("[Request interrupted by user]")
                    }
                    _ => false,
                }
            }
            AbortMarker::CodexTurnAborted => {
                // Every codex rollout record but the earliest is wrapped in a
                // `payload`; matching the bare shape too costs nothing and
                // keeps this from drifting apart from a fixture written
                // without the wrapper.
                let item = value.get("payload").unwrap_or(value);
                if item.get("role").and_then(|r| r.as_str()) != Some("user") {
                    return false;
                }
                item.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|blocks| blocks.first())
                    .and_then(|block| block.get("text"))
                    .and_then(|t| t.as_str())
                    .is_some_and(|text| text.starts_with("<turn_aborted>"))
            }
            AbortMarker::PiAborted => {
                let Some(message) = value.get("message") else {
                    return false;
                };
                message.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && message.get("stopReason").and_then(|s| s.as_str()) == Some("aborted")
            }
        }
    }
}

impl FileShape {
    /// How this shape reads in a report — `spoolway agent verify` prints the
    /// accounting row back at whoever is checking it.
    pub fn label(&self) -> String {
        match self {
            FileShape::Exact => "<id>.jsonl".to_string(),
            FileShape::AfterUnderscore => "<prefix>_<id>.jsonl".to_string(),
            FileShape::OwnHome { under } => format!("<id>/{under}/**/*.jsonl"),
        }
    }
}

impl Store {
    /// How this store reads in a report — `spoolway agent verify` prints the
    /// accounting row back at whoever is checking it.
    pub fn label(&self) -> String {
        match self {
            Store::Transcript(file) => file.label(),
        }
    }
}

impl Format {
    pub fn label(&self) -> &'static str {
        match self {
            Format::Pi => "Pi",
            Format::AnthropicApi => "AnthropicApi",
            Format::Codex => "Codex",
        }
    }
}

/// How this kind's spend is read back, if it can be.
///
/// There is no table here any more. A kind is one row — [`crate::agent::
/// Adapter`] — spanning how it launches and how it is accounted for, and this
/// resolves the second half through the first. `None` means the kind carries
/// no accounting row: either spoolway does not know it at all, or it is a
/// known kind that ships deliberately unmetered. Every reading below returns
/// nothing in that case, which is the same answer they already give for a lane
/// whose transcript cannot be found, and every caller already handles it.
pub fn kind(name: &str) -> Option<&'static crate::agent::Accounting> {
    crate::agent::adapter(name)?.accounting.as_ref()
}

/// The interactive agent sessions this spoolway process is running inside.
///
/// Each is an agent kind and that session's own id. This is how planning and
/// queueing — work no dispatcher started — can still be attributed: the session
/// that ran `spoolway plan close` is the session that did the planning.
///
/// **All of them, not the first.** Agents launch agents: a codex session
/// started from inside a claude one inherits `$CLAUDE_CODE_SESSION_ID` and
/// exports `$CODEX_THREAD_ID` over the top of it, so a spoolway command run
/// down there really is inside both, and both really did spend. Returning one
/// would mean the outer session — first in the table, and the one *not* doing
/// the work — silently swallowed the enrolment of the inner. Banking is a
/// per-session delta, so enrolling both counts each once and neither twice.
pub fn ambient_sessions() -> Vec<(&'static str, String)> {
    crate::agent::ADAPTERS
        .iter()
        .filter_map(|adapter| {
            let var = adapter.accounting.as_ref()?.session_env?;
            let value = std::env::var(var).ok()?;
            let value = value.trim();
            // An exported-but-empty variable means no session, not a session
            // named the empty string.
            (!value.is_empty()).then(|| (adapter.kind, value.to_string()))
        })
        .collect()
}

/// What one transcript added up to.
pub struct Harvest {
    pub model: String,
    pub tokens: Tokens,
    pub turns: u32,
    /// Cost the agent reported itself. Trusted over the price map when present:
    /// a local lane's honest zero is a fact spoolway should not have to be told.
    pub cost_usd: Option<f64>,
    /// The largest single-turn context reading seen anywhere in the
    /// transcript — the same figure [`last_turn`] takes of the *last* turn,
    /// maximised over every deduped turn instead. A compaction drops the last
    /// turn's own reading back down, so this is how a lane that ran close to
    /// its window and then compacted is still told apart from one that never
    /// got close.
    pub ctx_peak: u64,
}

/// Read the transcript an agent wrote for `session`, if it can be found.
///
/// Absence is not an error. A lane may have failed before its first turn, or
/// the project's `args` may not pass `{session_id}` through at all — neither is
/// worth failing a dispatch pass over, and both simply produce no ledger line.
pub fn harvest(kind: &str, session: &str) -> Option<Harvest> {
    harvest_file(kind, &session_file(kind, session)?)
}

/// The last assistant turn a transcript records, in whichever of the two
/// shapes this agent writes — one read and one JSON-parse of the file. See
/// [`last_turn`], the public form built on it.
fn last_turn_at(kind: &str, path: &Path) -> Option<Turn> {
    let mut last: Option<Turn> = None;
    let mut announced = String::new();
    for value in records_at(kind, path)? {
        // The same two-line dance `read_transcript` does: on a kind that names
        // its model beside the turn rather than on it, the turn alone does not
        // know what ran it.
        if let Some(model) = turn_model(kind, &value) {
            announced = model;
            continue;
        }
        if let Some(mut turn) = read_turn(kind, &value) {
            if turn.model.is_empty() {
                turn.model.clone_from(&announced);
            }
            // A synthetic turn is not an answer — see [`SYNTHETIC_MODEL`] — so
            // it must not become "the last turn" a caller sizes the
            // conversation by.
            if turn.model == SYNTHETIC_MODEL {
                continue;
            }
            last = Some(turn);
        }
    }
    last
}

/// What a conversation still in flight looks like from outside it.
///
/// Every figure the board puts on a running row, from one read and one scan —
/// see [`live_of`], which is where the three being one pass is the point.
pub struct Live {
    /// What resuming would resend: the last turn's input, cache read and cache
    /// write. See [`last_turn`] for why a sum over the transcript is the wrong
    /// answer to "how big is this conversation".
    pub context: u64,
    /// Everything the conversation has spent so far, each request banked once.
    /// The same reading [`harvest`] takes of a settled lane, off a transcript
    /// still being written — so a caller can price the step in flight with the
    /// arithmetic that will price it when it settles, and the two agree.
    pub harvest: Harvest,
}

/// [`last_turn`]'s size reading plus the whole conversation's spend, from a
/// transcript already located.
///
/// One pass, because the caller is the board: it asks about every live lane,
/// on a timer, and a transcript is the largest file in a run. Requests are
/// banked once by id for the reason [`read_transcript`] does it — Claude Code
/// writes an assistant line per content block and repeats that request's usage
/// verbatim on each, so counting every line roughly doubles the output.
pub fn live_of(kind: &str, path: &Path) -> Option<Live> {
    let transcript = read_transcript(kind, path);
    Some(Live {
        context: transcript.context,
        harvest: transcript.total?,
    })
}

/// The size of `session`'s last turn, off a single file read and a single
/// scan.
///
/// The input, cache-read and cache-write of the last assistant turn — what a
/// provider already held in context to answer it, and so what resuming would
/// send again. Deliberately not [`Harvest::tokens`], which sums every turn: a
/// sum only grows, so it answers what a session cost rather than how big it
/// has become. The last turn's own usage is the one number a provider ever
/// reports that means "this is the conversation's size", because each
/// request restates the whole conversation as its input.
///
/// Consulted by `dispatch::carried_session`, which checks this against
/// `agents.<profile>.session_reuse_ctx` and the session's own age against
/// `models.<glob>.session_reuse_idle` — the second reading is [`touched_at`]'s,
/// not this function's, since it is a fact about the store rather than about
/// any one turn.
pub fn last_turn(kind: &str, session: &str) -> Option<u64> {
    let path = session_file(kind, session)?;
    let turn = last_turn_at(kind, &path)?;
    Some(turn.tokens.input + turn.tokens.cache_read + turn.tokens.cache_write())
}

/// Whether `session`'s transcript, for a lane of this `kind`, ends in a turn
/// a person aborted with Escape.
///
/// The one difference herdr's own status vocabulary cannot see: idle,
/// working, blocked, done and unknown answer exactly the same whether a turn
/// finished on its own or a keystroke cut it off — see
/// [`crate::agent::Adapter::abort_marker`], which this reads.
///
/// "No", never an error, for every reason this cannot be answered: a kind
/// with no established marker, a session whose transcript cannot be found,
/// and an empty transcript.
///
/// Nothing calls this yet — the dispatcher still settles a person's Escape
/// exactly like a finished turn, and wiring that up is the next task's. Kept
/// public and tested rather than removed: this is the whole point of
/// landing it ahead of the caller.
#[allow(dead_code)]
pub fn last_turn_aborted(kind: &str, session: &str) -> bool {
    let Some(home) = home_dir() else {
        return false;
    };
    last_turn_aborted_in(&home, kind, session)
}

/// The same reading, against an explicit home — see [`session_file_in`],
/// which this is tested the same way as.
fn last_turn_aborted_in(home: &Path, kind: &str, session: &str) -> bool {
    let Some(marker) = crate::agent::adapter(kind).and_then(|a| a.abort_marker) else {
        return false;
    };
    let Some(path) = session_file_in(home, kind, session) else {
        return false;
    };
    let Some(records) = records_at(kind, &path) else {
        return false;
    };
    records.last().is_some_and(|record| marker.matches(record))
}

/// When `session`'s transcript was last written to, in epoch seconds.
///
/// The one honest reading of "has this agent said anything lately". A turn
/// appends — the model's message, each tool call, each tool result — and a
/// session wedged inside one long tool call appends nothing. That is the
/// question the dispatcher's reminder loop is asking, and the transcript is
/// where it can be asked without looking at a screen: see
/// `dispatch::note_progress`.
///
/// `None` for a lane with no transcript yet, or none that can be found — an
/// agent still starting up, a project whose `args` drop `{session_id}`. Absence
/// is not silence and not progress; it is the caller's to decide what to do
/// with, and the dispatcher measures from the launch instead.
///
/// The mtime rather than the last record's own timestamp: only one of the two
/// is a fact about *this machine's* clock, and a stalled clock comparison
/// against a provider's timestamps is a watchdog that fires on a timezone.
pub fn last_written(kind: &str, session: &str) -> Option<i64> {
    let modified = touched_at(&session_file(kind, session)?)?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).timestamp())
}

/// When this session's store last moved.
///
/// A plain file mtime — every remaining [`Store`] is one file, read a line at
/// a time or as a whole stream, so there is no sidecar to fold in the way a
/// database in WAL mode would need.
///
/// Public because the board asks the same question before paying to re-read —
/// see `status::live_session`, which must gate on the same reading or it would
/// cache a turn's first answer for the whole of that turn.
pub fn touched_at(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

fn harvest_file(kind: &str, path: &Path) -> Option<Harvest> {
    read_transcript(kind, path).total
}

/// One stretch of a transcript, and what it came to.
///
/// A session is not one thing. It runs a skill, finishes, sits idle, runs
/// another — and the transcript says so, in file order, which is the whole
/// reason spoolway needs no cooperation from a skill to label its spend.
pub struct Segment {
    /// The skill running when this stretch was spent, or
    /// [`INTERACTIVE_SKILL`].
    pub skill: String,
    pub harvest: Harvest,
}

/// A transcript read once: the whole thing, and the same thing partitioned.
///
/// Both come out of one pass because they are one pass — the dedupe of a
/// request repeated across content blocks has to be global, or a request whose
/// blocks straddle a marker would be banked in two segments.
struct Transcript {
    /// `None` when the file holds no assistant turn at all.
    total: Option<Harvest>,
    segments: Vec<Segment>,
    /// The last turn's own size — its input, cache read and cache write, which
    /// is what resuming this conversation would resend. Not derivable from
    /// [`total`](Transcript::total), which sums every turn: see [`last_turn`].
    context: u64,
}

/// The skill markers, and the totals, of one transcript.
///
/// The cursor starts at [`INTERACTIVE_SKILL`] and is moved by every slash
/// command in the file, to that command's own name — stripped of a
/// `spoolway-` prefix where it has one, kept as typed otherwise. A marker
/// says where a skill *started* and never where it ended, so a session that
/// carries on after one keeps attributing to it until the next marker
/// arrives.
fn read_transcript(kind: &str, path: &Path) -> Transcript {
    let none = Transcript {
        total: None,
        segments: Vec::new(),
        context: 0,
    };
    let Some(values) = records_at(kind, path) else {
        return none;
    };

    let mut tokens = Tokens::default();
    let mut model = String::new();
    let mut turns = 0u32;
    let mut context = 0u64;
    // The running maximum of every deduped turn's own `context` reading,
    // alongside `context` itself — the last turn's alone can't tell a lane
    // that compacted after peaking close to its window from one that never
    // did. See [`Harvest::ctx_peak`].
    let mut ctx_peak = 0u64;
    let mut reported = 0.0f64;
    let mut any_cost = false;
    // One request, several records: Claude Code writes an assistant line per
    // content block — text, thinking, each tool call — and repeats that
    // request's usage verbatim on every one of them. In a real transcript half
    // the assistant lines are such repeats, so counting them all roughly
    // doubles a lane's reported spend. Bank each request once.
    let mut seen: HashSet<String> = HashSet::new();
    // The same repeat, from a kind whose format mints no id to bank a
    // request by at all — `seen` above has nothing to catch it on, and
    // without this every one of those repeats got counted as its own turn.
    // Consecutive records with identical model, tokens and cost are that
    // request restating itself; anything that differs is a genuinely new
    // turn. Only the immediately preceding turn is compared, not every turn
    // seen so far, because two *different* requests that happen to cost the
    // same are not the repeat this exists to catch.
    let mut last_unidentified: Option<(String, Tokens, Option<f64>)> = None;

    // Kept in first-appearance order rather than a map: a reader of the ledger
    // sees the skills in the order the session ran them.
    let mut segments: Vec<Segment> = Vec::new();
    let mut cursor = INTERACTIVE_SKILL.to_string();
    // The model a kind announces beside its turns rather than on them. Moves
    // like `cursor` does, and for the same reason: the line that says it is not
    // the line that spends.
    let mut announced = String::new();

    for value in values {
        if let Some(command) = slash_command(kind, &value) {
            cursor = skill_of(&command);
            continue;
        }
        if let Some(model) = turn_model(kind, &value) {
            announced = model;
            continue;
        }
        let Some(mut turn) = read_turn(kind, &value) else {
            continue;
        };
        if turn.model.is_empty() {
            turn.model = announced.clone();
        }
        // A synthetic turn — see [`SYNTHETIC_MODEL`] — names no model that
        // ever spent, and its all-zero usage is not the conversation's real
        // size. Drop it before it can overwrite `model` or `context` below.
        if turn.model == SYNTHETIC_MODEL {
            continue;
        }
        if let Some(id) = turn.id {
            if !seen.insert(id) {
                continue;
            }
            // An identified turn breaks any run of fingerprint-matched
            // no-id turns before it — the comparison below only means
            // anything between genuinely consecutive records.
            last_unidentified = None;
        } else {
            let fingerprint = (turn.model.clone(), turn.tokens, turn.cost);
            if last_unidentified.as_ref() == Some(&fingerprint) {
                continue;
            }
            last_unidentified = Some(fingerprint);
        }

        tokens.add(&turn.tokens);
        turns += 1;
        // Overwritten by every turn, so what survives the loop is the last
        // one's — the only reading that means "how big has this got".
        context = turn.tokens.input + turn.tokens.cache_read + turn.tokens.cache_write();
        ctx_peak = ctx_peak.max(context);
        if !turn.model.is_empty() {
            model = turn.model.clone();
        }
        if let Some(cost) = turn.cost {
            reported += cost;
            any_cost = true;
        }

        let segment = match segments.iter_mut().find(|s| s.skill == cursor) {
            Some(segment) => segment,
            None => {
                segments.push(Segment {
                    skill: cursor.clone(),
                    harvest: Harvest {
                        model: String::new(),
                        tokens: Tokens::default(),
                        turns: 0,
                        cost_usd: None,
                        // Nothing reads a per-skill peak today — only the
                        // whole-transcript `total` below feeds the ledger's
                        // `ctx_peak` — so it is left at zero rather than
                        // tracked and unused.
                        ctx_peak: 0,
                    },
                });
                segments.last_mut().expect("just pushed")
            }
        };
        segment.harvest.tokens.add(&turn.tokens);
        segment.harvest.turns += 1;
        if !turn.model.is_empty() {
            segment.harvest.model = turn.model;
        }
        if let Some(cost) = turn.cost {
            segment.harvest.cost_usd = Some(segment.harvest.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    if turns == 0 {
        return none;
    }

    Transcript {
        total: Some(Harvest {
            model,
            tokens,
            turns,
            ctx_peak,
            cost_usd: any_cost.then_some(reported),
        }),
        segments,
        context,
    }
}

/// The skill a slash command names: whatever the command is actually called.
/// `spoolway-plan` is `spoolway-plan`, `my-plan` is `my-plan`, `clear` is
/// `clear` — nothing is rewritten on the way in, so the name in the ledger is
/// the name a person types. Which of those get a block of their own under
/// `spoolway eval` is a later decision, [`crate::config::Config::skills`]'s
/// to make; this only records what actually ran, so a name left off that list
/// today is still banked under its real name and can be promoted to a block
/// just by adding it.
///
/// A `spoolway-` prefix used to be stripped here, so that `/spoolway-plan`
/// banked as `plan`. It meant `config.toml` named one skill by a name that
/// appeared nowhere else — not on the command, not in `.claude/skills/`. The
/// lines that shape wrote are still read as `spoolway-plan`, in
/// [`Entry::skill_label`], so an existing ledger still totals to the same
/// money.
fn skill_of(command: &str) -> String {
    command.to_string()
}

/// The slash command a transcript record announces, if it announces one.
///
/// Claude Code writes an invocation as an ordinary timestamped user record
/// whose content carries `<command-name>/spoolway-plan</command-name>`, ahead
/// of the assistant turns that command spent. pi's transcript carries no such
/// marker, so a pi session is one undivided `interactive` stretch.
fn slash_command(kind_name: &str, value: &serde_json::Value) -> Option<String> {
    if kind(kind_name)?.format != Format::AnthropicApi {
        return None;
    }
    if value.get("type")?.as_str()? != "user" {
        return None;
    }
    let content = value.get("message")?.get("content")?;
    // A user message is a string, or a list of blocks. Only a text block can
    // carry the marker — a tool result is a block too, and a `grep` for
    // `<command-name>` is a thing a session really does.
    let text = match content.as_str() {
        Some(text) => text.to_string(),
        None => content
            .as_array()?
            .iter()
            .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
    };

    // The whole record is the command envelope, not prose that mentions one.
    // Which of `<command-name>` and `<command-message>` comes first has
    // changed between Claude Code versions, so this matches the envelope
    // rather than either order of it.
    if !text.trim_start().starts_with("<command-") {
        return None;
    }

    let open = "<command-name>";
    let start = text.find(open)? + open.len();
    let end = text[start..].find("</command-name>")? + start;
    let name = text[start..end].trim().trim_start_matches('/').trim();
    (!name.is_empty()).then(|| name.to_string())
}

struct Turn {
    id: Option<String>,
    model: String,
    tokens: Tokens,
    cost: Option<f64>,
}

/// Pull one assistant turn out of a transcript line, in whichever of the two
/// shapes this agent writes.
fn read_turn(kind_name: &str, value: &serde_json::Value) -> Option<Turn> {
    let num = |v: &serde_json::Value, key: &str| v.get(key).and_then(|n| n.as_u64()).unwrap_or(0);

    // Deliberately exhaustive, with no wildcard arm: adding a `Format` should
    // fail to compile here rather than silently account every lane of that
    // agent as zero.
    match kind(kind_name)?.format {
        // pi: `{"type":"message","id":..,"message":{"role":"assistant",
        //      "model":..,"usage":{"input":..,"cacheRead":..,"cost":{..}}}}`
        Format::Pi => {
            let message = value.get("message")?;
            if message.get("role")?.as_str()? != "assistant" {
                return None;
            }
            let usage = message.get("usage")?;
            Some(Turn {
                id: value.get("id").and_then(|v| v.as_str()).map(str::to_string),
                model: message
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tokens: Tokens {
                    input: num(usage, "input"),
                    output: num(usage, "output"),
                    cache_read: num(usage, "cacheRead"),
                    // pi reports one cache-write figure and prices its own
                    // transcripts, so the lifetime split does not arise; the
                    // cheaper class is the honest place to put it.
                    cache_write_5m: num(usage, "cacheWrite"),
                    cache_write_1h: 0,
                    reasoning: num(usage, "reasoning"),
                },
                cost: usage
                    .get("cost")
                    .and_then(|c| c.get("total"))
                    .and_then(|t| t.as_f64()),
            })
        }

        // claude: `{"type":"assistant","requestId":..,"message":{"model":..,
        //          "usage":{"input_tokens":..,"cache_read_input_tokens":..}}}`
        Format::AnthropicApi => {
            if value.get("type")?.as_str()? != "assistant" {
                return None;
            }
            let message = value.get("message")?;
            let usage = message.get("usage")?;
            // `cache_creation` breaks the write down by the cache's lifetime,
            // which is what decides its price. `cache_creation_input_tokens` is
            // the same total undifferentiated, and is all a transcript from
            // before that breakdown existed has — bill that at the five-minute
            // rate rather than guessing it was the dearer hour.
            let (write_5m, write_1h) = match usage.get("cache_creation") {
                Some(detail) => (
                    num(detail, "ephemeral_5m_input_tokens"),
                    num(detail, "ephemeral_1h_input_tokens"),
                ),
                None => (num(usage, "cache_creation_input_tokens"), 0),
            };
            Some(Turn {
                id: value
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                model: message
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
                tokens: Tokens {
                    input: num(usage, "input_tokens"),
                    output: num(usage, "output_tokens"),
                    cache_read: num(usage, "cache_read_input_tokens"),
                    cache_write_5m: write_5m,
                    cache_write_1h: write_1h,
                    reasoning: 0,
                },
                // Claude Code records no cost in its transcript, so this one is
                // the price map's job.
                cost: None,
            })
        }

        // codex: `{"type":"event_msg","payload":{"type":"token_count","info":{
        //          "last_token_usage":{"input_tokens":..,"cached_input_tokens":..}}}}`
        Format::Codex => {
            let payload = value.get("payload")?;
            if value.get("type")?.as_str()? != "event_msg"
                || payload.get("type")?.as_str()? != "token_count"
            {
                return None;
            }
            // `last_token_usage` is this turn; `total_token_usage` beside it is
            // the conversation so far. The per-turn one, because that is what
            // every other kind reports and what `read_transcript` sums — and
            // the two agree: over a three-turn rollout the `last` figures summed
            // to the final `total` exactly.
            let usage = payload.get("info")?.get("last_token_usage")?;

            // codex nests rather than partitions: `input_tokens` is the whole
            // input, with `cached_input_tokens` a subset of it. `Tokens` wants
            // the classes disjoint — they are summed, and each is priced at its
            // own rate — so the cached part is carved out rather than added
            // beside. Carved saturating, so a transcript that ever reported a
            // subset larger than its whole yields zero fresh input rather than
            // an underflow.
            //
            // The same is assumed of `cache_write_input_tokens`, and less
            // firmly: it was zero on every turn probed, so nothing observed
            // says which side of `input_tokens` it falls. Treating it as inside
            // is what keeps `Tokens::total` equal to codex's own `total_tokens`,
            // which was exact on every turn where it could be checked.
            let input = num(usage, "input_tokens");
            let cache_read = num(usage, "cached_input_tokens");
            let cache_write = num(usage, "cache_write_input_tokens");
            Some(Turn {
                // No per-request id, and none is needed: codex writes one
                // `token_count` per *request*, each carrying that request's own
                // usage — a task that calls a tool has two, and their `last`
                // figures sum to the running `total_token_usage` exactly
                // (7,525 + 7,786 = 15,311, and on to 31,560 over four records
                // in one rollout). So there are no repeats to bank away the way
                // Claude Code's per-content-block lines need.
                id: None,
                // Not on this record. The model is carried by the
                // `turn_context` line that precedes it — see [`turn_model`],
                // which is how `read_transcript` fills it in.
                model: String::new(),
                tokens: Tokens {
                    input: input.saturating_sub(cache_read).saturating_sub(cache_write),
                    output: num(usage, "output_tokens"),
                    cache_read,
                    // Reported as one figure with no lifetime attached, so it
                    // goes to the cheaper class for the reason pi's does.
                    cache_write_5m: cache_write,
                    cache_write_1h: 0,
                    reasoning: num(usage, "reasoning_output_tokens"),
                },
                // codex records no cost in its rollout — the price map's job.
                cost: None,
            })
        }
    }
}

/// The model a transcript line announces, for a kind that names it somewhere
/// other than on the turn itself.
///
/// codex's `token_count` carries no model; the `turn_context` record that
/// precedes each turn does, and it is per-turn rather than per-session because
/// a session really can change model partway. Read the same way
/// [`slash_command`] is — a line that moves a cursor rather than one that is a
/// turn.
fn turn_model(kind_name: &str, value: &serde_json::Value) -> Option<String> {
    if kind(kind_name)?.format != Format::Codex {
        return None;
    }
    if value.get("type")?.as_str()? != "turn_context" {
        return None;
    }
    let model = value.get("payload")?.get("model")?.as_str()?.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// Every record of one session, in file order.
///
/// Everything above this — [`read_transcript`], [`last_turn_at`], [`live_of`]
/// — works on the `Vec<Value>` this returns and never learns which [`Store`]
/// it came out of.
///
/// `None` means the store could not be read at all: no transcript yet, an
/// agent still starting up. Absence is never an error here — see
/// [`harvest`].
fn records_at(kind_name: &str, path: &Path) -> Option<Vec<serde_json::Value>> {
    // The guard that makes an unrecognised kind read as "nothing yet" rather
    // than as a real transcript: every remaining `Store` reads a plain file,
    // so nothing below this line needs to know which one — but a kind
    // `kind()` cannot resolve still must not fall through to reading a file
    // that happens to sit at `path`.
    kind(kind_name)?;
    Some(
        std::fs::read_to_string(path)
            .ok()?
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
    )
}

/// A settled lane's transcript, read back as plain text — the fallback
/// `spoolway lane` reaches for once there is no live pane left to read, on a
/// lane whose kind and session id are already known (from the lane records,
/// or the ledger, by way of `dispatch::lane_session`).
///
/// Best-effort, the same way the rest of accounting is: each record is
/// rendered by whatever text field it carries — `text`, `content`, or a
/// nested `message`, tried in that order and recursively through arrays and
/// objects — and a record with none of those falls back to its own compact
/// JSON rather than being dropped silently. `None` only when no transcript
/// can be found at all, which is exactly [`harvest`]'s own answer for that.
pub fn transcript_tail(kind: &str, session: &str, lines: usize) -> Option<String> {
    transcript_tail_in(&home_dir()?, kind, session, lines)
}

/// [`transcript_tail`], against an explicit home rather than this machine's
/// own — the split every other lookup in this module makes, and what lets a
/// test point it at a scratch tree instead of `$HOME`.
fn transcript_tail_in(home: &Path, kind: &str, session: &str, lines: usize) -> Option<String> {
    let path = session_file_in(home, kind, session)?;
    let records = records_at(kind, &path)?;
    let rendered: Vec<String> = records.iter().map(render_record).collect();
    let tail: Vec<&str> = rendered
        .iter()
        .rev()
        .take(lines)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(tail.join("\n"))
}

/// One transcript record, rendered to a line of plain text. See
/// [`transcript_tail`].
fn render_record(value: &serde_json::Value) -> String {
    fn text_of(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            serde_json::Value::Object(map) => ["text", "content", "message"]
                .into_iter()
                .find_map(|key| map.get(key).and_then(text_of)),
            serde_json::Value::Array(items) => items.iter().find_map(text_of),
            _ => None,
        }
    }
    text_of(value).unwrap_or_else(|| value.to_string())
}

/// Where an agent leaves the transcript for a session id spoolway pinned.
///
/// Both agents shard sessions into a directory named after the lane's working
/// directory, escaped in a scheme of their own. Rather than reimplement two
/// escaping rules that can change under us, look for the session id itself —
/// it is a UUID spoolway minted, so a match is unambiguous wherever it landed.
fn session_file(kind: &str, session: &str) -> Option<PathBuf> {
    session_file_in(&home_dir()?, kind, session)
}

/// The same lookup, for a caller that wants to know whether the transcript has
/// changed before paying to read it.
///
/// The search itself is a directory walk over every session an agent has ever
/// written, so a caller asking about once a second — the board — is
/// expected to hold on to what this returns rather than ask again.
pub fn session_path(kind: &str, session: &str) -> Option<PathBuf> {
    session_file(kind, session)
}

/// The lookup itself, against an explicit home, so it is testable without
/// touching the environment of a process that has lanes running in it.
fn session_file_in(home: &Path, kind_name: &str, session: &str) -> Option<PathBuf> {
    let agent = kind(kind_name)?;
    let Store::Transcript(file) = agent.store;
    let suffix = match file {
        FileShape::Exact => format!("{session}.jsonl"),
        FileShape::AfterUnderscore => format!("_{session}.jsonl"),
        // The name spoolway chose is the directory, so everything under it is
        // this session's by construction. Deepest-newest rather than first
        // found: the tree is date-sharded by the agent, and a session that ran
        // past midnight has two days' directories in it.
        FileShape::OwnHome { under } => {
            let mine = state_root_in(home).join(agent.sessions_dir).join(session);
            // The home is made at lane start, before the agent is told to use
            // it, so it exists from the first moment this id means a lane —
            // which makes it the one honest question to ask here. Asked before
            // the search rather than after it, so a lane whose first turn has
            // not landed yet answers "not written yet" instead of falling
            // through to a walk of every session on the machine.
            if mine.is_dir() {
                return newest_transcript(&mine.join(under));
            }
            // No home of spoolway's making, so this id is not one spoolway
            // minted: it is the agent's own, read out of the environment of an
            // interactive session. Its transcript is in the agent's home, in a
            // tree that holds every session it ever wrote — so here the id is
            // in the filename after all, at the end of it:
            // `rollout-<ts>-<id>.jsonl`. Matched on the stem's end rather than
            // on a separator, because which character joins them is the
            // agent's business and the id ending the name is what makes the
            // match unambiguous.
            let own = crate::agent::own_home_in(home, kind_name)?.join(under);
            return newest_matching(&own, |stem| stem.ends_with(session));
        }
    };

    for dir in std::fs::read_dir(home.join(agent.sessions_dir))
        .ok()?
        .flatten()
    {
        if !dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in entries.flatten() {
            let name = file.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(&suffix) {
                return Some(file.path());
            }
        }
    }
    None
}

/// The most recently modified `.jsonl` anywhere under `root`.
///
/// A walk rather than a glob because the shape of the tree below a session's
/// own home is the agent's business, not spoolway's — codex shards by date
/// today, and a version that shards by something else would still land here.
fn newest_transcript(root: &Path) -> Option<PathBuf> {
    newest_matching(root, |_| true)
}

/// The same walk, over the transcripts whose file stem `keep` accepts.
///
/// Newest rather than first found for both callers, and for the same reason in
/// each: under a home of spoolway's making every transcript is the session's,
/// and in the agent's own home a session that outlives a rollover has more than
/// one file carrying its id. The last thing written is the live one either way.
fn newest_matching(root: &Path, keep: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(_) if path.extension().is_some_and(|ext| ext == "jsonl") => {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                    if !keep(&stem) {
                        continue;
                    }
                    let Ok(at) = entry.metadata().and_then(|m| m.modified()) else {
                        continue;
                    };
                    if best.as_ref().is_none_or(|(best, _)| at > *best) {
                        best = Some((at, path));
                    }
                }
                _ => {}
            }
        }
    }
    best.map(|(_, path)| path)
}

/// Where spoolway keeps state of its own, for the one thing that needs a
/// directory rather than a file — see [`crate::agent::session_home`].
///
/// Deliberately from the home directory alone, and **not** from
/// `$XDG_STATE_HOME` the way [`registry::path`] reads it. The two are not the
/// same kind of path: the registry is written and read by the same command in
/// the same shell, while this one is written by a dispatcher and read back
/// later by whoever runs `spoolway eval --by` — possibly a different shell, a cron
/// entry, or a lane. An environment variable set for one and not the other
/// would send the reader looking in a directory the writer never used, and the
/// only symptom would be a lane that silently never appears in the ledger.
/// Reproducibility matters more here than honouring an override.
pub fn state_root() -> Option<PathBuf> {
    Some(state_root_in(&home_dir()?))
}

fn state_root_in(home: &Path) -> PathBuf {
    home.join(".local/state").join("spoolway")
}

/// A v4-shaped UUID, which is what `claude --session-id` insists on.
///
/// From the kernel's entropy, with a clock-and-pid fallback so that a machine
/// without `/dev/urandom` still gets distinct ids rather than a hard failure on
/// the launch path of every lane.
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    // Exactly sixteen bytes. `/dev/urandom` never reaches EOF, so anything that
    // reads to the end of it reads until the machine runs out of memory.
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64;
        let pid = std::process::id() as u64;
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..].copy_from_slice(&(pid.wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes());
    }
    // Version 4, variant 1.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// A fresh run id: `r` plus sixteen hex digits.
///
/// Never travels onto a lane name — a run id is stored on the task's own
/// frontmatter and copied onto ledger lines, not folded into `<task> · <step>`
/// — so there is no width budget to leave room in, and nothing gained by
/// keeping it short. 64 bits of entropy instead of 20 pushes a collision
/// (`group_by_run` would silently fold two unrelated runs into one row) from
/// something that starts showing up within a project's lifetime to something
/// that will not happen. From the same entropy [`new_session_id`] reads, so
/// it costs nothing to distinguish a run from another minted the same
/// second.
pub fn new_run_id() -> String {
    let mut bytes = [0u8; 8];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0) as u64;
        let pid = std::process::id() as u64;
        bytes = (nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes();
    }
    let value = u64::from_le_bytes(bytes);
    format!("r{value:016x}")
}

pub fn ledger_path(repo: &Repo) -> PathBuf {
    repo.usage_file()
}

/// Append one line. Opened, written and closed each time: a ledger written once
/// per finished lane is not worth a held file handle, and a short write is one
/// line lost rather than a corrupted file.
///
/// Every command banks to this ledger, once per invocation, and each lane
/// banks again on top of that once per step — see `main.rs:180` and
/// `src/dispatch.rs:1651` — so a project with a few lanes running has several
/// processes appending at once, routinely rather than rarely. A `File` opened
/// in append mode makes each individual `write` atomic relative to every
/// other appender — the kernel puts it at the file's current end and moves
/// the end past it as one step — but `writeln!` on it is *two* writes, the
/// line and its trailing newline, and a second appender's write can land
/// between them. One assembled buffer and one `write_all` is what keeps the
/// per-write atomicity the file mode already offers from being split back
/// open by this function.
pub fn append(repo: &Repo, entry: &Entry) -> Result<()> {
    let path = ledger_path(repo);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut line = serde_json::to_string(entry).context("serialising a usage entry")?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Every entry, oldest first. A line that will not parse is skipped rather than
/// fatal — one bad line must not cost you the rest of the history.
pub fn read(repo: &Repo) -> Result<Vec<Entry>> {
    read_at(&ledger_path(repo))
}

pub fn read_at(path: &Path) -> Result<Vec<Entry>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    Ok(raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .collect())
}

/// What [`read_cached`] last read: the entries it parsed, and how many bytes
/// of the file they came from — the offset the next call resumes from.
struct LedgerCache {
    path: PathBuf,
    len: u64,
    entries: std::sync::Arc<Vec<Entry>>,
}

/// [`read`], cached process-wide and refreshed only past what was already
/// read, exploiting the one thing [`append`] guarantees: the ledger only
/// ever grows, one full line at a time. For a reader that asks the same
/// question every frame — the board, above all, whose own comment used to
/// call re-reading a 40MB ledger a second the most expensive thing in a pass
/// that decides nothing — this turns most calls into "did the file get
/// bigger" rather than a full re-parse.
///
/// Returns a shared handle rather than an owned `Vec`: a warm cache that
/// found nothing new hands back a clone of the `Arc` — a refcount bump — not
/// a copy of however many entries the ledger holds. Only a call that finds
/// the file has actually grown pays to build a new `Vec`, and that cost is
/// proportional to what changed, once, not to the whole history on every
/// frame that finds nothing new.
///
/// A one-shot command is no worse off using this than calling [`read`]
/// directly: the first call is a full read either way, and the cache dies
/// with the process. `read` stays what banking and pricing logic reaches
/// for, since a single pass wants one settled answer to diff its own writes
/// against rather than a boundary that can move under it mid-pass.
///
/// Falls back to a full read whenever the file is shorter than what is
/// cached — rotated, truncated, or simply gone — rather than trust an
/// offset a shrunk file can no longer support. Errors read as empty, the
/// same as `read(..).unwrap_or_default()` every caller of this already
/// wrote.
pub fn read_cached(repo: &Repo) -> std::sync::Arc<Vec<Entry>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<LedgerCache>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    let path = ledger_path(repo);

    let Ok(mut guard) = cache.lock() else {
        return std::sync::Arc::new(read_at(&path).unwrap_or_default());
    };
    if let Some(cached) = guard.as_mut()
        && cached.path == path
        && let Some((tail, len)) = read_tail(&path, cached.len)
    {
        // Nothing new: the length did not move, so the existing `Arc` is
        // still the right answer and cloning it costs nothing but a
        // refcount. `read_tail` still runs — it is one `stat` plus a seek to
        // confirm that, cheap next to a 40MB parse — so this is the common
        // case a board sitting idle between passes actually takes.
        if !tail.is_empty() {
            let mut merged = (*cached.entries).clone();
            merged.extend(tail);
            cached.entries = std::sync::Arc::new(merged);
        }
        cached.len = len;
        return std::sync::Arc::clone(&cached.entries);
    }

    let entries = std::sync::Arc::new(read_at(&path).unwrap_or_default());
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    *guard = Some(LedgerCache {
        path,
        len,
        entries: std::sync::Arc::clone(&entries),
    });
    entries
}

/// The entries `path` has gained since byte `from`, and the file's length
/// once the read finished — measured from what was actually read rather
/// than a separate `stat` taken first, so a writer appending mid-read can
/// never be double-counted or skipped over. `None` when the file is shorter
/// than `from` — it was rotated or truncated out from under the offset —
/// which tells [`read_cached`] to fall back to a full read instead of
/// trusting an offset that no longer means anything.
fn read_tail(path: &Path, from: u64) -> Option<(Vec<Entry>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    if file.seek(SeekFrom::End(0)).ok()? < from {
        return None;
    }
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    let entries = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .collect();
    Some((entries, from + raw.len() as u64))
}

/// A half-open window `[from, until)` over the ledger.
///
/// Half-open because the alternative is arithmetic on the last second of a day,
/// and every off-by-one in date filtering lives there. `--until 2026-08-31`
/// means the whole of the 31st, which is `< 2026-09-01T00:00`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Window {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Window {
    pub fn contains(&self, ts: &str) -> bool {
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(ts) else {
            // A line whose timestamp will not parse cannot be placed in time.
            // Dropping it from every window is the honest answer; keeping it
            // would put it in windows it may not belong to.
            return false;
        };
        let at = at.with_timezone(&chrono::Utc);
        self.from.is_none_or(|from| at >= from) && self.until.is_none_or(|until| at < until)
    }
}

/// Parse one end of a window: a duration ago, an absolute local date, or a
/// whole month.
///
/// `30m`, `4h`, `2d6h` are durations back from now. `2026-08-01` is that date
/// in *your* timezone, because a person asking what August cost means their
/// August, not UTC's. `2026-08` is that whole month: the same half-open rule
/// that turns an `until` date into the start of the next day turns an
/// `until` month into the start of the next month, via [`parse_month`].
pub fn parse_instant(raw: &str, end_of_day: bool) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;

    let raw = raw.trim();
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        // The end of a day is the start of the next one, so that the bound
        // stays half-open and no second of the 31st is lost.
        let date = if end_of_day {
            date.succ_opt().context("date out of range")?
        } else {
            date
        };
        let naive = date.and_hms_opt(0, 0, 0).context("invalid date")?;
        return match chrono::Local.from_local_datetime(&naive).earliest() {
            Some(local) => Ok(local.with_timezone(&chrono::Utc)),
            // A midnight that does not exist locally, which daylight saving
            // really does produce. The following instant is the honest bound.
            None => Ok(chrono::Local
                .from_local_datetime(&naive)
                .latest()
                .map(|l| l.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now)),
        };
    }

    // `YYYY-MM` shaped, so try it as a month before falling to a duration —
    // `parse_month` already knows how to turn one into a window, and reusing
    // it here keeps the "not a real month" errors in one place. The shape
    // check is what keeps every other dashed string — `last-tuesday`,
    // `08-2026`, `2026-8` — out of `parse_month`: without it those would be
    // handed a month-shaped error instead of the duration one they should
    // get, since `parse_month` itself does no shape checking of its own.
    if let Some((year, month)) = raw.split_once('-')
        && year.len() == 4
        && year.bytes().all(|b| b.is_ascii_digit())
        && month.len() == 2
        && month.bytes().all(|b| b.is_ascii_digit())
    {
        let window = parse_month(raw)?;
        return Ok(if end_of_day {
            window.until.expect("parse_month always sets until")
        } else {
            window.from.expect("parse_month always sets from")
        });
    }

    let duration = crate::config::parse_duration(raw).map_err(|err| {
        anyhow::anyhow!("{err}; or give a date as YYYY-MM-DD, or a month as YYYY-MM")
    })?;
    Ok(chrono::Utc::now() - chrono::Duration::from_std(duration)?)
}

/// Turn `YYYY-MM` into the window covering that whole calendar month, locally.
pub fn parse_month(raw: &str) -> Result<Window> {
    let raw = raw.trim();
    let (year, month) = raw
        .split_once('-')
        .context("a month is YYYY-MM, for example 2026-08")?;
    let year: i32 = year.parse().context("a month's year is four digits")?;
    let month: u32 = month.parse().context("a month is 01 to 12")?;

    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1)
        .with_context(|| format!("`{raw}` is not a real month"))?;
    // Rolling into the next year is what makes December work.
    let next = match month {
        12 => chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1),
        _ => chrono::NaiveDate::from_ymd_opt(year, month + 1, 1),
    }
    .context("month out of range")?;

    Ok(Window {
        from: Some(parse_instant(&first.to_string(), false)?),
        until: Some(parse_instant(&next.to_string(), false)?),
    })
}

/// The calendar month an entry falls in, locally, as `YYYY-MM`.
pub fn month_of(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(at) => at.with_timezone(&chrono::Local).format("%Y-%m").to_string(),
        Err(_) => "?".to_string(),
    }
}

/// Every project this machine has run spoolway in.
///
/// A ledger lives inside its project, which is right — it is that project's
/// state, and two projects sharing one file would be a coordination problem
/// nobody asked for. The cost of that choice is that nothing knows where the
/// ledgers are, so a question spanning projects cannot be asked at all.
///
/// This is the index that fixes it, and deliberately nothing more: a list of
/// roots. It holds no usage of its own, so it can be deleted at any time and
/// the worst that happens is `--all` forgets a project until its next dispatch.
pub mod registry {
    use super::*;

    /// `$XDG_STATE_HOME/spoolway/projects.json`, or the default state directory.
    ///
    /// State rather than config: this is something spoolway observed, not
    /// something anyone configured, and it is not worth syncing between
    /// machines — the paths on another machine are not these paths.
    pub fn path() -> Option<PathBuf> {
        let base = match std::env::var_os("XDG_STATE_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => home_dir()?.join(".local/state"),
        };
        Some(base.join("spoolway").join("projects.json"))
    }

    /// Note that `root` is a spoolway project, if it is not already known.
    ///
    /// Best-effort throughout: a read-only home, a corrupt index, a racing
    /// dispatcher — none of them are worth failing a command over, because
    /// nothing in the pipeline depends on this file.
    pub fn register(root: &Path) {
        let Some(path) = path() else { return };
        let mut known = list_at(&path);
        let root = root.to_path_buf();
        if known.contains(&root) {
            return;
        }
        known.push(root);
        known.sort();
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        if let Ok(body) = serde_json::to_string_pretty(&known) {
            // Written whole rather than appended: it is a set, not a log, and
            // a torn write of a few paths costs nothing to rebuild.
            let _ = crate::task::write_atomic(&path, format!("{body}\n"));
        }
    }

    /// Known project roots that still look like projects, oldest registration
    /// order aside — a root whose `.spoolway` is gone was moved or deleted, and
    /// silently skipping it is better than reporting a total that omits it
    /// without saying so.
    pub fn list() -> Vec<PathBuf> {
        let Some(path) = path() else {
            return Vec::new();
        };
        list_at(&path)
            .into_iter()
            .filter(|root| root.join(crate::config::STATE_DIR).is_dir())
            .collect()
    }

    fn list_at(path: &Path) -> Vec<PathBuf> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<PathBuf>>(&raw).ok())
            .unwrap_or_default()
    }

    /// What a project is called on a report: the last component of its path.
    pub fn name_of(root: &Path) -> String {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string())
    }
}

/// Whether this spoolway process is a lane's rather than a person's.
///
/// A lane is itself a claude session and exports a session id of its own, so
/// without this guard every lane would bank its own spend a second time — once
/// as the step it ran, and once as skill spend. `dispatch::ENV_STEP` is the
/// one fact only a lane's environment carries.
fn in_lane() -> bool {
    std::env::var_os(crate::dispatch::ENV_STEP).is_some_and(|value| !value.is_empty())
}

/// Bank what one interactive session has spent since it was last banked, one
/// line per skill segment, and return what was appended.
///
/// Unlike a lane, this session is **still running** when it is read — so what
/// lands is a running total caught up, not a closed account. What it is *not*
/// is double counted: the ledger is the record of what was banked, so it is
/// also the check, and re-reading a session that is fully banked appends
/// nothing. That is what makes this safe to call on every command and every
/// read.
pub fn bank_session(repo: &Repo, kind: &str, session: &str) -> Vec<Entry> {
    let Some(path) = session_file(kind, session) else {
        return Vec::new();
    };
    bank_session_at(repo, kind, session, &path)
}

/// [`bank_session`] against an explicit transcript, so the banking rules are
/// testable without a home directory full of real sessions in them.
fn bank_session_at(repo: &Repo, kind_name: &str, session: &str, path: &Path) -> Vec<Entry> {
    let segments = read_transcript(kind_name, path).segments;

    // What this session has already been banked for, per skill.
    let mut banked: BTreeMap<String, (Tokens, u32, f64)> = BTreeMap::new();
    // Whether this ledger has ever heard of the session at all, which is a
    // different question from what it has been banked for: a session enrolled
    // and not yet spending has a line and no tokens on it.
    let mut enrolled = false;
    for entry in read(repo).unwrap_or_default() {
        if entry.session != session {
            continue;
        }
        enrolled = true;
        let Some(label) = entry.skill_label() else {
            continue;
        };
        if entry.skill.is_none() {
            // A line from before skills were labelled: the whole session was
            // banked under one label, so its segments cannot be told apart
            // from what is already on the ledger. Re-banking any of them would
            // count that session twice, and the old line is not rewritten —
            // leave the session alone, in full.
            return Vec::new();
        }
        let slot = banked.entry(label.to_string()).or_default();
        slot.0.add(&entry.tokens);
        slot.1 += entry.turns;
        slot.2 += entry.cost_usd.unwrap_or(0.0);
    }

    let stamp = crate::version::stamp(repo);
    // One line of this session, however it came to be banked. A closure rather
    // than two constructions, because every field below is a property of *the
    // session* — which task it belongs to (none), what it reports (nothing) —
    // and the two paths must not be able to drift on any of them.
    let line = |skill: &str, model: String, turns: u32, tokens: Tokens, cost_usd: Option<f64>| {
        Entry {
            ts: chrono::Utc::now().to_rfc3339(),
            // A skill session belongs to no task, and the transcript says
            // which skill ran but never which plan it was about.
            task: String::new(),
            plan: None,
            // Left empty rather than reused: a pipeline is free to declare a
            // step called `plan`, and the two must never merge into one row.
            step: String::new(),
            pipeline: String::new(),
            agent: INTERACTIVE_AGENT.to_string(),
            kind: kind_name.to_string(),
            model,
            session: session.to_string(),
            round: 0,
            // A session's wall time is how long you had the window open, which
            // is not a measure of anything. Left at zero rather than invented.
            wall_s: 0,
            turns,
            tokens,
            cost_usd,
            // A skill session is banked per segment, not per lane — nothing
            // reads a peak for one, so it is left absent the way a line
            // written before this field existed is.
            ctx_peak: None,
            version: Some(stamp.version.clone()),
            commit: stamp.commit.clone(),
            // A conversation reports no outcome, and inventing a `pass` would
            // put work in the pass rate that was never judged.
            outcome: None,
            // It belongs to no task, so it belongs to no run either.
            run: None,
            trial: None,
            skill: Some(skill.to_string()),
            project: String::new(),
        }
    };

    // A transcript with no turn spoolway can read yet — and a session that has
    // never been banked, so nothing will come back for it.
    //
    // This is not a hypothetical. codex writes a turn's `token_count` when the
    // turn *ends*, and a command run from inside that turn is by definition
    // running before it does: a rollout read at that moment carries the session
    // and not one usage record. Without a line here, the first command in a
    // codex session banks nothing, [`sweep`] never learns the session exists,
    // and the whole conversation goes unaccounted on the strength of *when* it
    // was asked rather than what it spent.
    //
    // So the enrolment is separated from the spend: a zero line, written once,
    // saying only that this session is one to keep counting. It invents no
    // number — the tokens really are the ones read, and every later sweep banks
    // the deltas on top of it as usual.
    if segments.is_empty() {
        if enrolled {
            return Vec::new();
        }
        let entry = line(INTERACTIVE_SKILL, String::new(), 0, Tokens::default(), None);
        return match append(repo, &entry) {
            Ok(()) => vec![entry],
            Err(_) => Vec::new(),
        };
    }

    let mut written = Vec::new();
    for segment in segments {
        let (already, turns, cost) = banked.get(&segment.skill).cloned().unwrap_or_default();
        let tokens = segment.harvest.tokens.since(&already);
        if tokens.is_zero() {
            // Nothing has happened under this skill since it was last banked.
            continue;
        }

        let model = segment.harvest.model.clone();
        // Prices are linear in tokens, so pricing the delta is the same as
        // differencing two priced totals — and it stays right when the agent
        // reports its own cost instead.
        let cost_usd = match segment.harvest.cost_usd {
            Some(total) => Some((total - cost).max(0.0)),
            None => price(&repo.config.models, &model, &tokens),
        };

        let entry = line(
            &segment.skill,
            model,
            segment.harvest.turns.saturating_sub(turns),
            tokens,
            cost_usd,
        );
        if append(repo, &entry).is_ok() {
            written.push(entry);
        }
    }
    written
}

/// Enrol the session this command is running in, by banking what it has spent
/// so far.
///
/// Called on every command, because nothing else tells spoolway that a session
/// exists at all: a banked line *is* the enrolment, and [`sweep`] catches the
/// session up afterwards from anywhere. Silent, and best-effort — a person
/// running `spoolway queue list` is not asking about money.
pub fn bank_ambient(repo: &Repo) -> Vec<Entry> {
    if in_lane() {
        return Vec::new();
    }
    ambient_sessions()
        .into_iter()
        .flat_map(|(kind, session)| bank_session(repo, kind, &session))
        .collect()
}

/// Catch every interactive session this ledger knows about up to its
/// transcript, and return what that appended.
///
/// This is what makes reading the ledger enough: a session is enrolled by the
/// first spoolway command run in it and swept by every read afterwards, so a
/// plan that was never queued — and the hour of conversation after the last
/// command — are still counted. Idempotent by construction, since
/// [`bank_session`] banks only the delta.
///
/// This project's ledger only. A `--all` read spans projects, but writing to
/// another project's ledger from a command run here is not something a read
/// should ever do.
pub fn sweep(repo: &Repo) -> Vec<Entry> {
    if in_lane() {
        return Vec::new();
    }

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut sessions: Vec<(String, String)> = Vec::new();
    for entry in read(repo).unwrap_or_default() {
        if !entry.is_skill() || entry.session.is_empty() {
            continue;
        }
        // A kind with no accounting row has no transcript to catch up to —
        // including one whose row was removed under a ledger that still
        // carries its old lines.
        if kind(&entry.kind).is_none() {
            continue;
        }
        if seen.insert((entry.kind.clone(), entry.session.clone())) {
            sessions.push((entry.kind, entry.session));
        }
    }

    sessions
        .into_iter()
        .flat_map(|(kind, session)| bank_session(repo, &kind, &session))
        .collect()
}

/// Read one project's ledger, tagging every entry with the project's name.
pub fn read_project(root: &Path) -> Vec<Entry> {
    let name = registry::name_of(root);
    let path = crate::mux::project_home(root).join(LEDGER_FILE);
    let mut entries = read_at(&path).unwrap_or_default();
    for entry in &mut entries {
        entry.project = name.clone();
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only: [`last_turn`]'s size reading, taken off an already-located
    /// transcript rather than a session lookup.
    fn last_turn_size_at(kind: &str, path: &Path) -> Option<u64> {
        let tokens = last_turn_at(kind, path)?.tokens;
        Some(tokens.input + tokens.cache_read + tokens.cache_write())
    }

    fn prices() -> BTreeMap<String, ModelPrice> {
        BTreeMap::from([
            (
                "claude-*".to_string(),
                ModelPrice {
                    context_window: 0,
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write_5m: 3.75,
                    cache_write_1h: 6.0,
                    session_reuse_idle: None,
                    slots: 0,
                    exclusive: false,
                },
            ),
            (
                "claude-opus-*".to_string(),
                ModelPrice {
                    context_window: 0,
                    input: 5.0,
                    output: 25.0,
                    cache_read: 0.5,
                    cache_write_5m: 6.25,
                    cache_write_1h: 10.0,
                    session_reuse_idle: None,
                    slots: 0,
                    exclusive: false,
                },
            ),
        ])
    }

    #[test]
    fn the_most_specific_pattern_prices_a_model() {
        let tokens = Tokens {
            input: 1_000_000,
            ..Tokens::default()
        };
        // Both patterns match; the one with more literal characters wins.
        assert_eq!(price(&prices(), "claude-opus-5", &tokens), Some(5.0));
        assert_eq!(price(&prices(), "claude-sonnet-5", &tokens), Some(3.0));
    }

    #[test]
    fn an_unpriced_model_costs_nothing_known_rather_than_nothing() {
        let tokens = Tokens {
            input: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(price(&prices(), "Qwen3.6-35B-A3B", &tokens), None);
    }

    /// A model this project never configured a glob for is still priced, from
    /// the vendored built-in table — the whole point of shipping one.
    #[test]
    fn a_model_no_glob_names_still_prices_from_the_built_in_table() {
        let tokens = Tokens {
            input: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(
            price(&BTreeMap::new(), "claude-haiku-4-5-20251001", &tokens),
            Some(1.0)
        );
    }

    #[test]
    fn every_priced_token_class_is_charged_at_its_own_rate() {
        let tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            reasoning: 500_000,
        };
        let cost = price(&prices(), "claude-opus-5", &tokens).unwrap();
        // Reasoning is already inside output and must not be charged twice, and
        // the two cache-write lifetimes are charged at their own rates.
        assert!(
            (cost - (5.0 + 25.0 + 0.5 + 6.25 + 10.0)).abs() < 1e-9,
            "{cost}"
        );
    }

    #[test]
    fn reasoning_tokens_are_not_added_to_the_total() {
        let tokens = Tokens {
            input: 10,
            output: 20,
            cache_read: 30,
            cache_write_5m: 25,
            cache_write_1h: 15,
            reasoning: 15,
        };
        assert_eq!(tokens.total(), 100);
    }

    /// The two cache lifetimes are 1.25× and 2× the input rate. Charging an
    /// hour write at the five-minute rate is the understatement this split
    /// exists to fix, so hold the arithmetic apart explicitly.
    #[test]
    fn an_hour_cache_write_costs_more_than_a_five_minute_one() {
        let five = Tokens {
            cache_write_5m: 1_000_000,
            ..Tokens::default()
        };
        let hour = Tokens {
            cache_write_1h: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(price(&prices(), "claude-opus-5", &five), Some(6.25));
        assert_eq!(price(&prices(), "claude-opus-5", &hour), Some(10.0));
    }

    /// A config written before the split has no hourly rate. Falling back to
    /// the five-minute one under-reports; treating the missing rate as free
    /// would drop the class entirely, which is worse.
    #[test]
    fn a_config_without_an_hourly_rate_falls_back_rather_than_charging_nothing() {
        let legacy = BTreeMap::from([(
            "claude-opus-5".to_string(),
            ModelPrice {
                context_window: 0,
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write_5m: 6.25,
                cache_write_1h: 0.0,
                session_reuse_idle: None,
                slots: 0,
                exclusive: false,
            },
        )]);
        let hour = Tokens {
            cache_write_1h: 1_000_000,
            ..Tokens::default()
        };
        assert_eq!(price(&legacy, "claude-opus-5", &hour), Some(6.25));
    }

    /// `cache_write` was one field before the split. An old ledger line must
    /// still read back, and its tokens must land somewhere rather than vanish.
    #[test]
    fn a_ledger_line_from_before_the_split_still_reads() {
        let raw = r#"{"ts":"2026-08-04T07:00:00+00:00","task":"login","step":"review",
            "pipeline":"default","agent":"claude","kind":"claude","model":"claude-opus-5",
            "session":"s","tokens":{"input":2,"output":516,"cache_read":42609,"cache_write":2169}}"#;
        let entry: Entry = serde_json::from_str(raw).expect("legacy line did not parse");
        assert_eq!(
            entry.tokens.cache_write_5m, 2169,
            "folded into the cheaper class"
        );
        assert_eq!(entry.tokens.cache_write_1h, 0);
        assert_eq!(entry.tokens.cache_write(), 2169);
    }

    /// Claude Code writes an hour cache by default, so the breakdown — not the
    /// flat total beside it — is what a current transcript must be read from.
    #[test]
    fn a_claude_turn_reads_the_cache_lifetime_breakdown() {
        let value = serde_json::json!({
            "type": "assistant",
            "requestId": "req_1",
            "message": {
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 268,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 21342,
                    "cache_creation": {
                        "ephemeral_1h_input_tokens": 21342,
                        "ephemeral_5m_input_tokens": 0
                    }
                }
            }
        });
        let turn = read_turn("claude", &value).unwrap();
        assert_eq!(turn.tokens.cache_write_1h, 21342);
        assert_eq!(turn.tokens.cache_write_5m, 0);
        // And the flat total is not added on top of the breakdown.
        assert_eq!(turn.tokens.cache_write(), 21342);
    }

    #[test]
    fn globs_anchor_at_both_ends() {
        assert!(glob_match("claude-opus-*", "claude-opus-5"));
        assert!(!glob_match("claude-opus-*", "xclaude-opus-5"));
        assert!(glob_match("*opus*", "claude-opus-5"));
        assert!(glob_match("claude-opus-5", "claude-opus-5"));
        assert!(!glob_match("claude-opus-5", "claude-opus-51"));
        assert!(!glob_match("gpt-*", "claude-opus-5"));
    }

    #[test]
    fn a_pi_transcript_sums_its_turns_and_keeps_its_own_zero_cost() {
        let line = |input: u64, output: u64, id: &str| {
            serde_json::json!({
                "type": "message",
                "id": id,
                "message": {
                    "role": "assistant",
                    "model": "Qwen3.6-35B-A3B",
                    "usage": {
                        "input": input, "output": output,
                        "cacheRead": 100, "cacheWrite": 0, "reasoning": 5,
                        "cost": {"total": 0.0}
                    }
                }
            })
        };
        let mut tokens = Tokens::default();
        let mut turns = 0;
        for value in [line(10, 20, "a"), line(30, 40, "b"), line(30, 40, "b")] {
            if let Some(turn) = read_turn("pi", &value) {
                // The repeated id is what dedupe drops; simulate that here.
                if turns < 2 {
                    tokens.add(&turn.tokens);
                }
                turns += 1;
            }
        }
        assert_eq!(tokens.input, 40);
        assert_eq!(tokens.output, 60);
        assert_eq!(tokens.cache_read, 200);
    }

    #[test]
    fn a_claude_turn_keeps_cached_and_fresh_input_apart() {
        let value = serde_json::json!({
            "type": "assistant",
            "requestId": "req_1",
            "message": {
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": 2,
                    "output_tokens": 516,
                    "cache_read_input_tokens": 42609,
                    "cache_creation_input_tokens": 2169
                }
            }
        });
        let turn = read_turn("claude", &value).unwrap();
        assert_eq!(turn.tokens.input, 2);
        assert_eq!(turn.tokens.cache_read, 42609);
        assert_eq!(turn.tokens.cache_write_5m, 2169);
        // Summing the classes must not double count the cached prefix.
        assert_eq!(turn.tokens.total(), 2 + 516 + 42609 + 2169);
        assert_eq!(turn.cost, None);
    }

    #[test]
    fn a_user_turn_is_not_a_turn() {
        let value = serde_json::json!({
            "type": "message",
            "id": "u1",
            "message": {"role": "user", "content": "hello"}
        });
        assert!(read_turn("pi", &value).is_none());
    }

    /// A scratch home laid out the way each agent lays out its own, holding one
    /// transcript under the session id spoolway would have pinned.
    fn home_with(kind: &str, session: &str, lines: &str) -> PathBuf {
        let root = crate::scratch::root(&format!("usage-{kind}-{session}"));
        let (dir, file) = match kind {
            // Both shard by an escaping of the lane's working directory, which
            // is exactly what the lookup must not have to reproduce.
            "pi" => (
                root.join(".pi/agent/sessions/--home-someone-work--"),
                format!("2026-08-04T06-14-15-743Z_{session}.jsonl"),
            ),
            // codex names the file itself, from an id of its own that spoolway
            // never sees; what spoolway named is the directory two levels up,
            // and the date shard below it is codex's business.
            "codex" => (
                root.join(".local/state/spoolway/codex")
                    .join(session)
                    .join("sessions/2026/08/13"),
                "rollout-2026-08-13T10-09-40-019ffa2b-76f7-7f71-a2bf-008fbc08d63d.jsonl"
                    .to_string(),
            ),
            _ => (
                root.join(".claude/projects/-home-someone-work"),
                format!("{session}.jsonl"),
            ),
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), lines).unwrap();
        root
    }

    /// Two turns in the shape codex actually writes, taken verbatim from a
    /// rollout — trimmed to the records that matter, with nothing renamed.
    const CODEX_TRANSCRIPT: &str = r#"{"timestamp":"2026-08-13T10:09:40.0Z","type":"session_meta","payload":{"session_id":"019ffa2b","cwd":"/home/someone/work","cli_version":"0.147.0","model_provider":"local"}}
{"timestamp":"2026-08-13T10:09:41.0Z","type":"turn_context","payload":{"turn_id":"t1","cwd":"/home/someone/work","model":"Muse-Glimmer-30B","approval_policy":"never"}}
{"timestamp":"2026-08-13T10:09:42.0Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1","model_context_window":258400}}
{"timestamp":"2026-08-13T10:09:59.0Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9630,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":76,"reasoning_output_tokens":0,"total_tokens":9706},"last_token_usage":{"input_tokens":9630,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":76,"reasoning_output_tokens":0,"total_tokens":9706}}}}
{"timestamp":"2026-08-13T10:10:15.0Z","type":"turn_context","payload":{"turn_id":"t2","cwd":"/home/someone/work","model":"Muse-Glimmer-30B","approval_policy":"never"}}
{"timestamp":"2026-08-13T10:10:16.0Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t2","model_context_window":258400}}
{"timestamp":"2026-08-13T10:10:31.0Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":19353,"cached_input_tokens":9705,"cache_write_input_tokens":0,"output_tokens":118,"reasoning_output_tokens":30,"total_tokens":19471},"last_token_usage":{"input_tokens":9723,"cached_input_tokens":9705,"cache_write_input_tokens":0,"output_tokens":42,"reasoning_output_tokens":30,"total_tokens":9765}}}}
"#;

    /// Two turns in the shape pi actually writes, taken from a real transcript.
    const PI_TRANSCRIPT: &str = r#"{"type":"session","id":"s","cwd":"/home/someone/work","timestamp":"2026-08-04T06:14:15.743Z","version":"1"}
{"type":"message","id":"02585358","parentId":"2b4db76f","timestamp":"2026-08-04T06:14:22.633Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}],"api":"openai-completions","provider":"qwen","model":"Qwen3.6-35B-A3B","usage":{"input":2336,"output":61,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":2397,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse"}}
{"type":"message","id":"u1","message":{"role":"user","content":"go on"}}
{"type":"message","id":"02585359","parentId":"02585358","timestamp":"2026-08-04T06:14:29.100Z","message":{"role":"assistant","content":[],"provider":"qwen","model":"Qwen3.6-35B-A3B","usage":{"input":26,"output":58,"cacheRead":6048,"cacheWrite":0,"reasoning":12,"totalTokens":6132,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"endTurn"}}
"#;

    /// The same, in the shape Claude Code writes — including one request split
    /// across two records with its usage repeated on both, which is what a turn
    /// containing both text and a tool call actually looks like on disk.
    const CLAUDE_TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user","content":"hi"},"uuid":"a","timestamp":"2026-08-04T06:14:15.743Z"}
{"type":"assistant","requestId":"req_1","uuid":"b","timestamp":"2026-08-04T06:14:20.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":268,"cache_read_input_tokens":0,"cache_creation_input_tokens":21342,"service_tier":"standard"}}}
{"type":"assistant","requestId":"req_1","uuid":"c","timestamp":"2026-08-04T06:14:21.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":268,"cache_read_input_tokens":0,"cache_creation_input_tokens":21342}}}
{"type":"assistant","requestId":"req_2","uuid":"d","timestamp":"2026-08-04T06:14:44.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":516,"cache_read_input_tokens":42609,"cache_creation_input_tokens":2169}}}
"#;

    /// The record claude's transcript ends in once Escape lands mid-turn,
    /// taken verbatim off a real one — see the plan this task comes from.
    const CLAUDE_ABORT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"uuid":"e","timestamp":"2026-08-04T06:14:50.000Z"}
"#;

    /// One of Claude Code's own locally generated messages — a spend-limit
    /// notice, an API error, an interrupt — recorded as an assistant turn
    /// under model `<synthetic>` with an all-zero `usage` block. Nothing
    /// prices `<synthetic>`, and its zero tokens are not the conversation's
    /// real size.
    const CLAUDE_SYNTHETIC: &str = r#"{"type":"assistant","requestId":"req_3","uuid":"g","timestamp":"2026-08-04T06:15:20.000Z","message":{"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
"#;

    /// The record codex's rollout ends in once Escape lands mid-turn, taken
    /// verbatim off a real one — see the plan this task comes from.
    const CODEX_ABORT: &str = r#"{"timestamp":"2026-08-13T10:10:40.0Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<turn_aborted>\nThe user interrupted the previous turn on purpose. Any running unified exec processes were terminated."}]}}
"#;

    /// The record pi's transcript ends in once Escape lands mid-turn, taken
    /// verbatim off a real one — see the plan this task comes from.
    const PI_ABORT: &str = r#"{"type":"message","id":"u2","parentId":"02585359","timestamp":"2026-08-04T06:15:00.000Z","message":{"role":"assistant","content":[],"stopReason":"aborted","errorMessage":"Operation aborted"}}
"#;

    /// A turn that landed normally after the abort — appended in the "not
    /// last" tests below, to prove a marker earlier in the file is not read
    /// as "the lane is parked now".
    const CLAUDE_FOLLOWUP: &str = r#"{"type":"assistant","requestId":"req_3","uuid":"f","timestamp":"2026-08-04T06:15:10.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":3,"output_tokens":10,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
"#;

    #[test]
    fn each_kinds_own_marker_is_read_off_the_real_record_it_writes() {
        let session = "0198e2c0-6666-4000-8000-000000000006";

        let home = home_with(
            "claude",
            session,
            &format!("{CLAUDE_TRANSCRIPT}{CLAUDE_ABORT}"),
        );
        assert!(
            last_turn_aborted_in(&home, "claude", session),
            "claude's own interrupt record must be recognised"
        );
        std::fs::remove_dir_all(&home).ok();

        let home = home_with(
            "codex",
            session,
            &format!("{CODEX_TRANSCRIPT}{CODEX_ABORT}"),
        );
        assert!(
            last_turn_aborted_in(&home, "codex", session),
            "codex's own <turn_aborted> record must be recognised"
        );
        std::fs::remove_dir_all(&home).ok();

        let home = home_with("pi", session, &format!("{PI_TRANSCRIPT}{PI_ABORT}"));
        assert!(
            last_turn_aborted_in(&home, "pi", session),
            "pi's own stopReason:aborted record must be recognised"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// An abort that is not the *last* record is history, not a parked lane
    /// — a turn landed on top of it, so the transcript no longer ends in an
    /// interrupt. Only checked against claude here: the matcher itself is
    /// per-kind, but "only the last record counts" is the reader's own logic
    /// and does not vary by kind.
    #[test]
    fn an_abort_that_is_not_the_last_record_answers_no() {
        let session = "0198e2c0-7777-4000-8000-000000000007";
        let home = home_with(
            "claude",
            session,
            &format!("{CLAUDE_TRANSCRIPT}{CLAUDE_ABORT}{CLAUDE_FOLLOWUP}"),
        );

        assert!(
            !last_turn_aborted_in(&home, "claude", session),
            "a turn written after the interrupt must win"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// A kind spoolway does not even know carries no marker at all — see
    /// [`crate::agent::Adapter::abort_marker`] — so this answers "no" with no
    /// transcript read at all, regardless of what is in it.
    #[test]
    fn a_kind_with_no_marker_answers_no() {
        let session = "0198e2c0-8888-4000-8000-000000000008";
        assert!(!last_turn_aborted_in(
            &crate::scratch::root("usage-no-marker"),
            "nosuchkind",
            session
        ));
    }

    /// No transcript on disk at all — a lane never launched, or one whose
    /// session cannot be found — answers "no" rather than panicking or
    /// erroring.
    #[test]
    fn a_transcript_that_cannot_be_found_answers_no() {
        assert!(!last_turn_aborted_in(
            &crate::scratch::root("usage-abort-no-transcript"),
            "claude",
            "0198e2c0-9999-4000-8000-000000000009",
        ));
    }

    /// A transcript that exists but holds nothing yet — a lane between start
    /// and its first written line — answers "no" rather than treating an
    /// empty file as an aborted one.
    #[test]
    fn an_empty_transcript_answers_no() {
        let session = "0198e2c0-aaaa-4000-8000-00000000000a";
        let home = home_with("claude", session, "");

        assert!(!last_turn_aborted_in(&home, "claude", session));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_pi_lane_is_found_by_the_session_spoolway_pinned_and_costs_nothing() {
        let session = "0198e2c0-1111-4000-8000-000000000001";
        let home = home_with("pi", session, PI_TRANSCRIPT);

        let path = session_file_in(&home, "pi", session).expect("transcript not found");
        let harvest = harvest_file("pi", &path).expect("nothing harvested");

        assert_eq!(harvest.model, "Qwen3.6-35B-A3B");
        assert_eq!(harvest.turns, 2, "the user turn must not count");
        assert_eq!(harvest.tokens.input, 2336 + 26);
        assert_eq!(harvest.tokens.output, 61 + 58);
        assert_eq!(harvest.tokens.cache_read, 6048);
        assert_eq!(harvest.tokens.reasoning, 12);
        // A local lane's zero is reported by the agent, not inferred by spoolway.
        assert_eq!(harvest.cost_usd, Some(0.0));

        std::fs::remove_dir_all(&home).ok();
    }

    /// `spoolway lane`'s fallback once there is no live pane: the same
    /// transcript `harvest` reads, rendered back as the text a person would
    /// actually want to read rather than the usage it adds up to.
    #[test]
    fn a_settled_lanes_transcript_reads_back_as_plain_text() {
        let session = "0198e2c0-4444-4000-8000-000000000004";
        let home = home_with("pi", session, PI_TRANSCRIPT);

        let text = transcript_tail_in(&home, "pi", session, 100).expect("nothing read back");
        assert!(text.contains("Qwen3.6-35B-A3B"), "{text}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// No transcript at all — never launched, or a kind `records_at` cannot
    /// read — answers `None` exactly as `harvest` does for the same lane,
    /// rather than an empty string a caller might mistake for "read, and
    /// empty".
    #[test]
    fn no_transcript_at_all_is_none_not_an_empty_read() {
        let home = crate::scratch::root("usage-no-transcript");
        assert_eq!(
            transcript_tail_in(&home, "pi", "0198e2c0-5555-4000-8000-000000000005", 100),
            None
        );
    }

    #[test]
    fn render_record_prefers_a_text_field_over_the_raw_json() {
        let value = serde_json::json!({"type": "assistant", "text": "hello there"});
        assert_eq!(render_record(&value), "hello there");
    }

    #[test]
    fn render_record_falls_back_to_compact_json_when_nothing_reads_as_text() {
        let value = serde_json::json!({"usage": {"input_tokens": 2}});
        assert_eq!(render_record(&value), value.to_string());
    }

    /// The lookup codex needed a different shape for: nothing in the filename
    /// is spoolway's, so what is matched is the directory above it.
    ///
    /// Worth stating what this proves beyond "a file was found": the transcript
    /// is named after an id codex minted, in a date-sharded tree codex chose,
    /// and neither appears anywhere in what spoolway asked for.
    #[test]
    fn a_codex_lane_is_found_by_the_home_spoolway_named_not_the_file_codex_did() {
        let session = "0198e2c0-3333-4000-8000-000000000003";
        let home = home_with("codex", session, CODEX_TRANSCRIPT);

        let path = session_file_in(&home, "codex", session).expect("transcript not found");
        assert!(
            path.file_name()
                .is_some_and(|name| !name.to_string_lossy().contains(session)),
            "the filename must not be what matched — codex named it, not spoolway"
        );
        assert!(path.starts_with(home.join(".local/state/spoolway/codex").join(session)));

        std::fs::remove_dir_all(&home).ok();
    }

    /// codex reports each turn's input *inclusive* of the part that was cached,
    /// and `Tokens` needs the classes disjoint because it sums them and prices
    /// each at its own rate. Counting the cached part twice would inflate every
    /// resumed turn — which is most of them — so this pins the carve-out.
    #[test]
    fn a_codex_turn_carves_the_cached_part_out_of_its_input() {
        let session = "0198e2c0-4444-4000-8000-000000000004";
        let home = home_with("codex", session, CODEX_TRANSCRIPT);

        let path = session_file_in(&home, "codex", session).expect("transcript not found");
        let harvest = harvest_file("codex", &path).expect("nothing harvested");

        assert_eq!(harvest.turns, 2, "one `token_count` per turn, and no more");
        // Turn two reported 9,723 input of which 9,705 was cached: 18 fresh.
        assert_eq!(harvest.tokens.input, 9630 + 18);
        assert_eq!(harvest.tokens.cache_read, 9705);
        assert_eq!(harvest.tokens.output, 76 + 42);
        // Counted inside `output`, so it must not be added to the total again.
        assert_eq!(harvest.tokens.reasoning, 30);
        assert_eq!(
            harvest.tokens.total(),
            9706 + 9765,
            "the summed turns must come to codex's own `total_tokens`"
        );
        // Not on the `token_count` record at all — carried by the
        // `turn_context` line ahead of it.
        assert_eq!(harvest.model, "Muse-Glimmer-30B");
        // codex records no cost of its own, so this is the price map's job.
        assert_eq!(harvest.cost_usd, None);

        std::fs::remove_dir_all(&home).ok();
    }

    /// The other half of [`FileShape::OwnHome`], and the one that closes the
    /// interactive gap: a session codex minted for *itself*, in the home codex
    /// chose, found by the id it exports as `$CODEX_THREAD_ID`.
    ///
    /// Everything spoolway pins is absent here — no home it made, no id it
    /// minted — so what has to work is the opposite lookup: the agent's own
    /// tree, holding every session it ever wrote, picked apart by the id at the
    /// end of the filename.
    #[test]
    fn an_interactive_codex_session_is_found_in_the_home_codex_chose() {
        let _ambient = AMBIENT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("CODEX_HOME");
        crate::platform::remove_test_env("CODEX_HOME");

        // Both ids are real ones codex minted, and both rollouts are named the
        // way codex named them.
        let session = "019ffa86-b076-78a0-9ee3-cda940041941";
        let other = "019ffa85-78c5-75c3-ba31-44a6799b9439";
        let home = crate::scratch::root(&format!("usage-codex-own-{session}"));
        let dir = home.join(".codex/sessions/2026/08/13");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("rollout-2026-08-13T11-49-18-{session}.jsonl")),
            CODEX_TRANSCRIPT,
        )
        .unwrap();
        // Written second, so it is the newest file in the tree: the id has to
        // be what picks, not the mtime.
        std::fs::write(
            dir.join(format!("rollout-2026-08-13T11-47-59-{other}.jsonl")),
            CODEX_TRANSCRIPT,
        )
        .unwrap();

        let path = session_file_in(&home, "codex", session).expect("transcript not found");
        assert!(
            path.file_name().is_some_and(|name| name
                .to_string_lossy()
                .ends_with(&format!("-{session}.jsonl"))),
            "the id ends the filename, and that is what matched"
        );
        assert_eq!(harvest_file("codex", &path).map(|h| h.turns), Some(2));

        // `CODEX_HOME` moves the tree for this lookup exactly as it moves it
        // for codex — a person who relocates their codex home keeps being
        // accounted for.
        let moved = home.join("elsewhere");
        let rollout = format!("sessions/2026/08/13/rollout-2026-08-13T12-00-00-{session}.jsonl");
        std::fs::create_dir_all(moved.join("sessions/2026/08/13")).unwrap();
        std::fs::write(moved.join(&rollout), CODEX_TRANSCRIPT).unwrap();
        crate::platform::set_test_env("CODEX_HOME", &moved);
        assert_eq!(
            session_file_in(&home, "codex", session),
            Some(moved.join(&rollout))
        );
        crate::platform::remove_test_env("CODEX_HOME");

        // And a home spoolway made for this id settles it the other way, even
        // empty: that home is made at lane start, so its existence means the id
        // is a lane's and its transcript is nowhere else. Without that question
        // asked first, every lane still waiting for its first turn would fall
        // through to a walk of every session on the machine.
        std::fs::create_dir_all(home.join(".local/state/spoolway/codex").join(session)).unwrap();
        assert_eq!(session_file_in(&home, "codex", session), None);

        std::fs::remove_dir_all(&home).ok();
        if let Some(value) = previous {
            crate::platform::set_test_env("CODEX_HOME", value);
        }
    }

    /// The reading `dispatch::carried_session` sizes a carried session by.
    #[test]
    fn a_codex_session_reports_its_size() {
        let session = "0198e2c0-5555-4000-8000-000000000005";
        let home = home_with("codex", session, CODEX_TRANSCRIPT);

        let path = session_file_in(&home, "codex", session).expect("transcript not found");
        // The last turn's own input is the conversation's size — 9,723, whether
        // it arrived cached or fresh — not the 19,353 the two turns sum to.
        assert_eq!(last_turn_size_at("codex", &path), Some(9723));

        let live = live_of("codex", &path).expect("no live reading");
        assert_eq!(live.context, 9723);
        assert_eq!(live.harvest.tokens.output, 76 + 42);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_claude_lane_is_priced_from_the_map_and_banks_each_request_once() {
        let session = "0198e2c0-2222-4000-8000-000000000002";
        let home = home_with("claude", session, CLAUDE_TRANSCRIPT);

        let path = session_file_in(&home, "claude", session).expect("transcript not found");
        let harvest = harvest_file("claude", &path).expect("nothing harvested");

        // req_1 spans two records and is banked once.
        assert_eq!(harvest.turns, 2);
        assert_eq!(harvest.tokens.output, 268 + 516);
        assert_eq!(harvest.tokens.cache_write(), 21342 + 2169);
        assert_eq!(harvest.tokens.cache_read, 42609);
        // req_2 is the fullest point: 2 fresh + 42609 read back from cache.
        // Claude Code records no cost, so the price map is what answers.
        assert_eq!(harvest.cost_usd, None);

        let cost = price(&prices(), &harvest.model, &harvest.tokens).expect("no price matched");
        let expected = (4.0 * 5.0 + 784.0 * 25.0 + 42609.0 * 0.5 + 23511.0 * 6.25) / 1e6;
        assert!((cost - expected).abs() < 1e-9, "{cost} vs {expected}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// `seen` above only catches a repeat by its id, and Claude Code is not
    /// the only format this reads — the next adapter added may write no id
    /// at all, the way codex's own `token_count` line does not. Two
    /// consecutive records with identical model, tokens and cost stand in
    /// for that adapter repeating one request's usage the same way Claude
    /// Code does per content block; a third, genuinely different record
    /// proves the fallback is not just collapsing everything id-less into
    /// one turn.
    #[test]
    fn a_repeated_turn_with_no_id_is_deduped_the_same_as_one_with_a_repeated_id() {
        let session = "0198e2c0-2222-4000-8000-000000000009";
        let transcript = r#"{"type":"user","message":{"role":"user","content":"hi"},"uuid":"a","timestamp":"2026-08-04T06:14:15.743Z"}
{"type":"assistant","uuid":"b","timestamp":"2026-08-04T06:14:20.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":268,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","uuid":"c","timestamp":"2026-08-04T06:14:21.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":268,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","uuid":"d","timestamp":"2026-08-04T06:14:44.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"output_tokens":516,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
"#;
        let home = home_with("claude", session, transcript);
        let path = session_file_in(&home, "claude", session).expect("transcript not found");
        let harvest = harvest_file("claude", &path).expect("nothing harvested");

        assert_eq!(
            harvest.turns, 2,
            "the repeated record must be banked once, and the different one kept"
        );
        assert_eq!(harvest.tokens.output, 268 + 516);

        std::fs::remove_dir_all(&home).ok();
    }

    /// A lane whose transcript ends on one of Claude Code's synthetic
    /// messages must still be banked under the model that actually answered,
    /// with the real last turn's size — not `<synthetic>` and not zero.
    #[test]
    fn a_synthetic_turn_contributes_no_model_context_or_turn_count() {
        let session = "0198e2c0-8888-4000-8000-000000000008";
        let home = home_with(
            "claude",
            session,
            &format!("{CLAUDE_TRANSCRIPT}{CLAUDE_SYNTHETIC}"),
        );
        let path = session_file_in(&home, "claude", session).expect("transcript not found");

        let harvest = harvest_file("claude", &path).expect("nothing harvested");
        assert_eq!(
            harvest.model, "claude-opus-5",
            "the synthetic turn must not overwrite the real model"
        );
        assert_eq!(
            harvest.turns, 2,
            "the synthetic turn must not be counted as a turn"
        );

        // req_2, the last *real* turn: 2 input, 42609 cache-read, 2169
        // cache-write. If the synthetic turn's all-zero usage is read as the
        // conversation's size, this comes back 0 instead.
        let size = last_turn_size_at("claude", &path).expect("nothing sized");
        assert_eq!(
            size,
            2 + 42609 + 2169,
            "a synthetic turn must not report the conversation as empty"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Sizing reads the *last* turn, not the sum of every turn `harvest_file`
    /// banks — the second pi record's own usage, not the two turns added
    /// together.
    #[test]
    fn last_turn_size_reads_the_final_turn_not_the_sum() {
        let session = "0198e2c0-4444-4000-8000-000000000004";
        let home = home_with("pi", session, PI_TRANSCRIPT);
        let path = session_file_in(&home, "pi", session).expect("transcript not found");

        let size = last_turn_size_at("pi", &path).expect("nothing sized");

        // The final turn alone: input 26, cacheRead 6048, no cache write.
        assert_eq!(size, 26 + 6048);

        std::fs::remove_dir_all(&home).ok();
    }

    /// The reason `ctx_peak` exists at all: a session that grows large and
    /// then comes back down — a compaction — must still be told apart from
    /// one that never got close. `PI_TRANSCRIPT`'s own two turns happen to
    /// grow, so this one shrinks instead: the first turn is the largest, the
    /// second and last is small, and `harvest_file`'s `ctx_peak` has to be
    /// the first's reading, not the last's.
    #[test]
    fn ctx_peak_is_the_largest_turn_even_when_it_is_not_the_last() {
        let shrinking = r#"{"type":"message","id":"t1","message":{"role":"assistant","model":"m","usage":{"input":90000,"output":10,"cacheRead":0,"cacheWrite":0}}}
{"type":"message","id":"t2","message":{"role":"assistant","model":"m","usage":{"input":100,"output":10,"cacheRead":0,"cacheWrite":0}}}
"#;
        let session = "0198e2c0-4444-4000-8000-000000000044";
        let home = home_with("pi", session, shrinking);
        let path = session_file_in(&home, "pi", session).expect("transcript not found");

        // The last turn alone is the small one — `last_turn_size_at` and
        // `ctx_peak` must disagree, or this is not testing anything.
        assert_eq!(last_turn_size_at("pi", &path), Some(100));

        let harvest = harvest_file("pi", &path).expect("nothing harvested");
        assert_eq!(harvest.ctx_peak, 90000);

        std::fs::remove_dir_all(&home).ok();
    }

    /// The board reads a live lane's transcript once and gets every figure
    /// off it: how big the conversation has got, what it has produced, and
    /// what it spent doing so. The last two are sums over every request, which
    /// is exactly where Claude Code's repeated assistant lines would double
    /// them if they were not banked once each.
    #[test]
    fn a_live_transcript_gives_its_size_and_its_spend_in_one_pass() {
        let session = "0198e2c0-6666-4000-8000-000000000006";
        let home = home_with("claude", session, CLAUDE_TRANSCRIPT);
        let path = session_file_in(&home, "claude", session).expect("transcript not found");

        let live = live_of("claude", &path).expect("nothing read");

        // The same last turn `last_turn_size_at` sizes, and the same totals
        // `harvest_file` banks — one read rather than two.
        assert_eq!(live.context, 2 + 42609 + 2169);
        assert_eq!(live.harvest.tokens.output, 268 + 516);

        let banked = harvest_file("claude", &path).expect("nothing harvested");
        assert_eq!(
            live.harvest.tokens.output, banked.tokens.output,
            "the board and the ledger must agree about one transcript"
        );
        // Priced the same way, off the same reading: what the board shows a
        // step running is what the ledger banks when it settles.
        assert_eq!(
            price(&prices(), &live.harvest.model, &live.harvest.tokens),
            price(&prices(), &banked.model, &banked.tokens)
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// The claude shape splits a cache write across two possible fields; a
    /// transcript with no `cache_creation` breakdown falls back to the
    /// undifferentiated one, exactly as `harvest_file` already does.
    #[test]
    fn last_turn_size_sums_input_cache_read_and_cache_write() {
        let session = "0198e2c0-5555-4000-8000-000000000005";
        let home = home_with("claude", session, CLAUDE_TRANSCRIPT);
        let path = session_file_in(&home, "claude", session).expect("transcript not found");

        let size = last_turn_size_at("claude", &path).expect("nothing sized");

        // req_2, the transcript's last record: 2 input, 42609 cache-read,
        // 2169 cache-write.
        assert_eq!(size, 2 + 42609 + 2169);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_session_that_was_never_written_is_absence_not_failure() {
        let home = crate::scratch::root("usage-empty");
        std::fs::create_dir_all(&home).unwrap();
        assert!(session_file_in(&home, "pi", "0198e2c0-3333-4000-8000-000000000003").is_none());
        assert!(harvest("nushell", "0198e2c0-3333-4000-8000-000000000003").is_none());
    }

    #[test]
    fn the_ledger_round_trips_and_survives_a_bad_line() {
        let dir = crate::scratch::root("usage-ledger");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.jsonl");
        std::fs::remove_file(&path).ok();

        let entry = Entry {
            ts: "2026-08-04T06:14:15+00:00".into(),
            task: "login".into(),
            plan: Some("auth".into()),
            step: "review".into(),
            pipeline: "default".into(),
            agent: "claude".into(),
            kind: "claude".into(),
            model: "claude-opus-5".into(),
            session: "s".into(),
            round: 2,
            wall_s: 214,
            turns: 9,
            tokens: Tokens {
                input: 2,
                output: 516,
                cache_read: 42609,
                cache_write_5m: 2169,
                cache_write_1h: 0,
                reasoning: 0,
            },
            cost_usd: Some(0.41),
            ctx_peak: None,
            version: Some("3f9a1c04".into()),
            commit: Some("a91c33e".into()),
            outcome: Some("pass".into()),
            run: Some("r00001".into()),
            trial: None,
            skill: None,
            project: String::new(),
        };
        let line = serde_json::to_string(&entry).unwrap();
        std::fs::write(&path, format!("{line}\nnot json at all\n\n{line}\n")).unwrap();

        let back = read_at(&path).unwrap();
        assert_eq!(back.len(), 2, "a bad line must cost only itself");
        assert_eq!(back[0].task, "login");
        assert_eq!(back[0].tokens.cache_read, 42609);
        assert_eq!(back[0].cost_usd, Some(0.41));
        assert_eq!(back[0].version.as_deref(), Some("3f9a1c04"));
        assert_eq!(back[0].outcome.as_deref(), Some("pass"));
        assert_eq!(back[0].run.as_deref(), Some("r00001"));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn minimal_entry(task: &str) -> Entry {
        Entry {
            ts: chrono::Utc::now().to_rfc3339(),
            task: task.to_string(),
            plan: None,
            step: "implement".into(),
            pipeline: "default".into(),
            agent: "pi".into(),
            kind: "pi".into(),
            model: "priced-model".into(),
            session: "s".into(),
            round: 0,
            wall_s: 0,
            turns: 1,
            tokens: Tokens::default(),
            cost_usd: None,
            ctx_peak: None,
            version: None,
            commit: None,
            outcome: None,
            run: None,
            trial: None,
            skill: None,
            project: String::new(),
        }
    }

    /// `read_cached` reads the ledger once and again only past what it has
    /// already read — proven here by reading it, appending, and reading
    /// again, rather than by reasoning about `append`'s own atomicity from a
    /// distance.
    #[test]
    fn read_cached_picks_up_only_what_was_appended_since() {
        let (repo, _) = fixture("read-cached");

        append(&repo, &minimal_entry("first")).unwrap();
        let after_first = read_cached(&repo);
        assert_eq!(after_first.iter().filter(|e| e.task == "first").count(), 1);

        append(&repo, &minimal_entry("second")).unwrap();
        let after_second = read_cached(&repo);
        assert_eq!(
            after_second.iter().filter(|e| e.task == "second").count(),
            1,
            "the second call must see what was appended after the first"
        );
        assert_eq!(
            after_second.iter().filter(|e| e.task == "first").count(),
            1,
            "and still carry what the first call already read"
        );
    }

    /// A line written before runs existed has none, and must still parse —
    /// the whole point of the fallback key `spoolway eval` derives for it.
    #[test]
    fn a_line_from_before_runs_existed_still_reads_with_no_run() {
        let raw = r#"{"ts":"2026-08-04T07:00:00+00:00","task":"login","step":"review",
            "pipeline":"default","agent":"claude","kind":"claude","model":"claude-opus-5",
            "session":"s","tokens":{"input":2,"output":516}}"#;
        let entry: Entry = serde_json::from_str(raw).expect("legacy line did not parse");
        assert_eq!(entry.run, None);
    }

    #[test]
    fn run_ids_are_wide_and_distinct() {
        let a = new_run_id();
        let b = new_run_id();
        assert!(a.starts_with('r'));
        assert_eq!(a.len(), 17, "{a}");
        // Not a strict guarantee — it is 64 bits of entropy — but a pair of
        // freshly minted ids colliding would be worth knowing about.
        assert_ne!(a, b);
    }

    /// A wall clock reading, here, as a ledger line would carry it.
    ///
    /// These windows are local by design — a person asking what August cost
    /// means their August — so the assertions have to be written in the
    /// runner's own timezone. Spelling a fixed `+02:00` instead only agrees
    /// with the clock in one place on earth, and CI is not that place.
    fn at(wall: &str) -> String {
        use chrono::TimeZone;
        let naive = chrono::NaiveDateTime::parse_from_str(wall, "%Y-%m-%dT%H:%M:%S")
            .expect("a wall clock reading is YYYY-MM-DDTHH:MM:SS");
        chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .expect("that wall clock reading does not exist here")
            .to_rfc3339()
    }

    #[test]
    fn an_absolute_date_bounds_the_whole_of_that_day() {
        let window = Window {
            from: Some(parse_instant("2026-08-01", false).unwrap()),
            until: Some(parse_instant("2026-08-31", true).unwrap()),
        };
        // The first instant of the 1st is in; the last of the 31st is too.
        assert!(window.contains(&at("2026-08-01T00:00:00")));
        assert!(window.contains(&at("2026-08-31T23:59:59")));
        // And the neighbouring days are not.
        assert!(!window.contains(&at("2026-07-31T23:59:59")));
        assert!(!window.contains(&at("2026-09-01T00:00:00")));
    }

    #[test]
    fn a_month_covers_itself_and_nothing_either_side() {
        let window = parse_month("2026-08").unwrap();
        assert!(window.contains(&at("2026-08-01T00:00:00")));
        assert!(window.contains(&at("2026-08-31T23:59:59")));
        assert!(!window.contains(&at("2026-07-31T23:59:59")));
        assert!(!window.contains(&at("2026-09-01T00:00:00")));
    }

    /// The month that rolls the year is the one an off-by-one hides in.
    #[test]
    fn december_rolls_into_the_next_year() {
        let window = parse_month("2026-12").unwrap();
        assert!(window.contains(&at("2026-12-31T23:00:00")));
        assert!(!window.contains(&at("2027-01-01T00:00:00")));
    }

    #[test]
    fn a_month_that_is_not_a_month_is_refused() {
        assert!(parse_month("2026-13").is_err());
        assert!(parse_month("2026").is_err());
        assert!(parse_month("august").is_err());
        assert!(parse_month("").is_err());
    }

    #[test]
    fn a_duration_and_a_date_are_both_accepted_as_a_bound() {
        // Durations stay relative to now.
        let hour_ago = parse_instant("1h", false).unwrap();
        assert!(hour_ago < chrono::Utc::now());
        assert!(hour_ago > chrono::Utc::now() - chrono::Duration::hours(2));
        // Dates are absolute.
        assert!(parse_instant("2026-08-01", false).is_ok());
        // And nonsense is refused with both forms named — both of which
        // `parse_instant` actually accepts, so the message no longer
        // recommends a form it would then turn around and reject.
        let err = parse_instant("last tuesday", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("YYYY-MM-DD"), "{err}");
        assert!(err.contains("month as YYYY-MM"), "{err}");
    }

    /// `parse_instant` takes `YYYY-MM` too, resolving it the same way
    /// `parse_month` would: the start of that month for a `since` bound, the
    /// start of the *next* month for an `until` bound, so `--since 2026-08`
    /// and `--until 2026-08` bound a month exactly the way a date bounds a
    /// day.
    #[test]
    fn a_month_is_accepted_as_a_bound_start_and_end() {
        assert_eq!(
            parse_instant("2026-08", false).unwrap(),
            parse_instant("2026-08-01", false).unwrap()
        );
        assert_eq!(
            parse_instant("2026-08", true).unwrap(),
            parse_instant("2026-09-01", false).unwrap()
        );
    }

    /// A month that is not real is refused the same way whether it is typed
    /// straight into `--month` or found inside `--since`/`--until`, and the
    /// error still says it is the month that is wrong.
    #[test]
    fn a_month_that_is_not_real_is_refused_through_parse_instant_too() {
        for bad in ["2026-13", "2026-00"] {
            let err = parse_instant(bad, false).unwrap_err().to_string();
            assert!(err.contains(bad), "{err}");
        }
    }

    #[test]
    fn an_unbounded_window_holds_everything() {
        let window = Window::default();
        // Written in UTC rather than locally: an unbounded window holds every
        // instant however it was written down, which is the point here.
        assert!(window.contains("2019-01-01T00:00:00Z"));
        assert!(window.contains("2099-01-01T00:00:00Z"));
        // But a timestamp that cannot be read cannot be placed in time.
        assert!(!window.contains("whenever"));
    }

    #[test]
    fn a_month_label_follows_local_time() {
        use chrono::{Offset, TimeZone};

        // 00:30 on the 1st, here. Written down two hours west the same instant
        // reads as the 31st of July — and the person who ran the lane would
        // still call it August, because that is the month they were in.
        let just_after_midnight = chrono::Local
            .with_ymd_and_hms(2026, 8, 1, 0, 30, 0)
            .single()
            .expect("00:30 on the 1st of August is a real local time");
        let two_hours_west = chrono::FixedOffset::east_opt(
            just_after_midnight.offset().fix().local_minus_utc() - 2 * 3600,
        )
        .expect("two hours west of here is still a real offset");
        let written_west = just_after_midnight
            .with_timezone(&two_hours_west)
            .to_rfc3339();
        assert!(
            written_west.starts_with("2026-07-31"),
            "the point of this test is a line whose own date disagrees: {written_west}"
        );

        assert_eq!(month_of(&written_west), "2026-08");
        assert_eq!(month_of("nonsense"), "?");
    }

    #[test]
    fn the_registry_keeps_one_entry_per_project_and_forgets_deleted_ones() {
        let home = crate::scratch::root("registry-test");
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        // Point the registry at a scratch state dir rather than the real one.
        let previous = std::env::var_os("XDG_STATE_HOME");
        crate::platform::set_test_env("XDG_STATE_HOME", &home);

        let live = home.join("live");
        std::fs::create_dir_all(live.join(crate::config::STATE_DIR)).unwrap();
        let gone = home.join("gone");

        registry::register(&live);
        registry::register(&live);
        registry::register(&gone);

        let listed = registry::list();
        assert_eq!(
            listed,
            vec![live.clone()],
            "registered twice or kept a ghost"
        );
        assert_eq!(registry::name_of(&live), "live");

        match previous {
            Some(value) => crate::platform::set_test_env("XDG_STATE_HOME", value),
            None => crate::platform::remove_test_env("XDG_STATE_HOME"),
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// The accounting half is reached through the adapter table now, so the
    /// invariant worth holding here is the resolution itself: a kind that
    /// declares an accounting row resolves to it, and a kind that declares
    /// none resolves to nothing rather than to a neighbour's transcript shape.
    /// The table's own invariants — one row per kind, and that these two
    /// answers agree — are asserted in `agent.rs`, where the table lives.
    #[test]
    fn accounting_resolves_only_for_a_kind_that_declares_it() {
        for adapter in crate::agent::ADAPTERS {
            assert_eq!(
                kind(adapter.kind).is_some(),
                adapter.meters(),
                "`{}` resolves differently from what its row declares",
                adapter.kind
            );
        }
        // Every shipped kind is metered now — the loop above already checks
        // that against each row's own declaration — so there is no longer a
        // known-but-unmetered kind to assert against, only an unknown one.
        assert!(
            kind("nosuchkind").is_none(),
            "an unknown kind must not resolve"
        );
    }

    /// The shipped agent profiles name kinds the ledger can actually read.
    /// Without this, a profile could be added whose lanes silently never
    /// appear in `spoolway eval --by`.
    ///
    /// Note what this does *not* say: that every launchable kind is metered.
    /// An unmetered kind is a legal state a project may adopt on purpose — see
    /// [`crate::agent::Accounting`]. It says only that the two profiles
    /// spoolway itself ships are ones whose spend it can read.
    #[test]
    fn every_shipped_profile_names_a_kind_the_ledger_understands() {
        for (name, profile) in crate::config::AgentProfile::defaults() {
            assert!(
                kind(&profile.kind).is_some(),
                "profile `{name}` has kind `{}`, which nothing can account for",
                profile.kind
            );
        }
    }

    /// A planning session is read while it is still running, so each close
    /// banks only what arrived since the last one. This is the arithmetic that
    /// keeps one session from being counted once per plan it closed.
    #[test]
    fn banking_a_session_twice_records_only_what_is_new() {
        let first = Tokens {
            input: 348,
            output: 126_300,
            cache_read: 26_080_000,
            cache_write_5m: 215_300,
            cache_write_1h: 0,
            reasoning: 0,
        };
        let later = Tokens {
            input: 352,
            output: 127_400,
            cache_read: 26_556_900,
            cache_write_5m: 217_000,
            cache_write_1h: 0,
            reasoning: 0,
        };

        let delta = later.since(&first);
        assert_eq!(delta.input, 4);
        assert_eq!(delta.output, 1_100);
        assert_eq!(delta.cache_write_5m, 1_700);

        // The whole point: the two bankings sum to the session, not to twice it.
        let mut total = first;
        total.add(&delta);
        assert_eq!(total, later);
    }

    #[test]
    fn a_session_with_nothing_new_banks_nothing() {
        let tokens = Tokens {
            input: 10,
            output: 20,
            ..Tokens::default()
        };
        assert!(tokens.since(&tokens).is_zero());
    }

    /// A truncated or rolled-over transcript can read lower than what was
    /// already banked. That must not credit the ledger with negative spend.
    #[test]
    fn a_total_that_went_backwards_yields_nothing_rather_than_a_credit() {
        let banked = Tokens {
            input: 100,
            output: 200,
            cache_read: 300,
            cache_write_5m: 400,
            cache_write_1h: 0,
            reasoning: 0,
        };
        let smaller = Tokens {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write_5m: 4,
            cache_write_1h: 0,
            reasoning: 0,
        };
        assert!(smaller.since(&banked).is_zero());
    }

    // ------------------------------------------------------------ skills

    /// A transcript in the shape Claude Code writes: a slash command as a user
    /// record, then the assistant turns it spent.
    fn transcript(records: &[(&str, u64)]) -> String {
        let mut out = String::new();
        for (n, (marker, output)) in records.iter().enumerate() {
            if !marker.is_empty() {
                out += &format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "user",
                        "message": {"role": "user", "content":
                            format!("<command-name>{marker}</command-name>\n<command-args></command-args>")},
                    })
                );
            }
            out += &format!(
                "{}\n",
                serde_json::json!({
                    "type": "assistant",
                    "requestId": format!("req-{n}"),
                    "message": {
                        "model": "claude-opus-5",
                        "usage": {"input_tokens": 10, "output_tokens": output},
                    },
                })
            );
        }
        out
    }

    fn fixture(name: &str) -> (Repo, PathBuf) {
        let root = crate::scratch::root(&format!("skill-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::config::STATE_DIR)).unwrap();
        let transcript = root.join("session.jsonl");
        let mut config = crate::config::Config::default();
        config.models.insert(
            "claude-*".to_string(),
            ModelPrice {
                output: 10.0,
                ..ModelPrice::default()
            },
        );
        let home = root.join("home");
        (
            Repo {
                checkout: root.clone(),
                root,
                config,
                home,
            },
            transcript,
        )
    }

    /// The whole mechanism, in one file: the markers say which skill was in
    /// the chair, and every turn after one belongs to it until the next.
    #[test]
    fn a_transcript_is_partitioned_by_the_skill_markers_in_it() {
        let (_, path) = fixture("segments");
        std::fs::write(
            &path,
            transcript(&[
                // Before any marker at all.
                ("", 1),
                ("/spoolway-plan", 2),
                // No marker: still planning, because a marker says where a
                // skill started and never where it ended.
                ("", 4),
                // A slash command that is not a spoolway skill ends it, and
                // banks under its own real name — as, now, does the spoolway
                // one above, rather than falling back to `interactive`.
                ("/clear", 8),
                ("/spoolway-queue", 16),
            ]),
        )
        .unwrap();

        let read = read_transcript("claude", &path);
        let by_skill: BTreeMap<&str, u64> = read
            .segments
            .iter()
            .map(|s| (s.skill.as_str(), s.harvest.tokens.output))
            .collect();

        assert_eq!(
            by_skill["spoolway-plan"], 6,
            "the turn after the marker is still planning, under the command's full name"
        );
        assert_eq!(
            by_skill["spoolway-queue"], 16,
            "the `spoolway-` prefix is no longer stripped"
        );
        assert_eq!(by_skill["clear"], 8, "a command banks under its own name");
        assert_eq!(
            by_skill["interactive"], 1,
            "only the stretch before the first marker"
        );
        // The whole file still totals to the whole file.
        assert_eq!(read.total.unwrap().tokens.output, 31);
    }

    /// A session's turns land under `interactive` when no skill has been
    /// invoked, which is what keeps an ordinary conversation accounted for.
    #[test]
    fn a_transcript_with_no_marker_is_one_interactive_stretch() {
        let (_, path) = fixture("plain");
        std::fs::write(&path, transcript(&[("", 3), ("", 5)])).unwrap();

        let segments = read_transcript("claude", &path).segments;
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].skill, INTERACTIVE_SKILL);
        assert_eq!(segments[0].harvest.turns, 2);
    }

    /// A session that greps its own transcript is a session this feature was
    /// built in. A marker is a command envelope, not any text that names one.
    #[test]
    fn a_tool_result_that_quotes_a_marker_is_not_one() {
        let (_, path) = fixture("quoted");
        let quoted = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "usage.rs:12: <command-name>/spoolway-plan</command-name>",
            }]},
        });
        // A text block that merely mentions one, too.
        let mentioned = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{
                "type": "text",
                "text": "the marker looks like <command-name>/spoolway-queue</command-name>",
            }]},
        });
        std::fs::write(
            &path,
            format!("{quoted}\n{mentioned}\n{}", transcript(&[("", 5)])),
        )
        .unwrap();

        let segments = read_transcript("claude", &path).segments;
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].skill, INTERACTIVE_SKILL);
    }

    /// The envelope Claude Code actually writes, in both the orders it has
    /// written it in.
    #[test]
    fn a_marker_is_read_whichever_way_round_the_envelope_is() {
        for content in [
            "<command-name>/spoolway-plan</command-name>\n<command-message>spoolway-plan</command-message>",
            "<command-message>spoolway-plan</command-message>\n<command-name>/spoolway-plan</command-name>\n<command-args>go</command-args>",
        ] {
            let value = serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": content},
            });
            assert_eq!(
                slash_command("claude", &value).as_deref(),
                Some("spoolway-plan")
            );
        }
    }

    /// The first command in a session, run before that session's first turn
    /// has been written down.
    ///
    /// codex writes a turn's `token_count` when the turn ends, and a command
    /// run from inside the turn runs before that — so the rollout it reads
    /// carries the session and no usage at all. Banking nothing there would
    /// leave the session unenrolled, and [`sweep`] only ever comes back for a
    /// session the ledger already names: the whole conversation would go
    /// unaccounted for having been asked a turn too early.
    #[test]
    fn a_session_with_nothing_readable_yet_is_still_enrolled() {
        let (repo, path) = fixture("enrol");
        // A rollout as it stands mid-turn: codex has opened the session and
        // named its model, and neither line is a turn.
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019ffa92\"}}\n\
             {\"type\":\"turn_context\",\"payload\":{\"model\":\"Muse-Glimmer-30B\"}}\n",
        )
        .unwrap();

        let enrolled = bank_session_at(&repo, "codex", "019ffa92", &path);
        assert_eq!(enrolled.len(), 1, "the session has to be on the ledger");
        assert!(enrolled[0].tokens.is_zero(), "and it has spent nothing yet");
        assert_eq!(enrolled[0].turns, 0);
        assert_eq!(enrolled[0].cost_usd, None, "no spend, and no price for it");
        assert!(enrolled[0].is_skill(), "it is a session, not a lane");
        // Enrolled once. A session that stays quiet does not grow a line per
        // command run in it.
        assert!(bank_session_at(&repo, "codex", "019ffa92", &path).is_empty());

        // And when the turn does land, it is banked in full on top — the zero
        // line consumed none of it.
        std::fs::write(&path, CODEX_TRANSCRIPT).unwrap();
        let banked = bank_session_at(&repo, "codex", "019ffa92", &path);
        assert_eq!(banked.len(), 1);
        assert_eq!(banked[0].tokens.output, 76 + 42);
        assert_eq!(banked[0].turns, 2);
        assert_eq!(banked[0].model, "Muse-Glimmer-30B");
    }

    /// Banking is a delta, per (session, skill), so a command run twice with
    /// nothing in between is a command that writes nothing.
    #[test]
    fn banking_a_session_again_appends_only_what_is_new() {
        let (repo, path) = fixture("delta");
        std::fs::write(&path, transcript(&[("/spoolway-plan", 100)])).unwrap();

        let first = bank_session_at(&repo, "claude", "s1", &path);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].skill.as_deref(), Some("spoolway-plan"));
        assert_eq!(first[0].tokens.output, 100);
        assert_eq!(first[0].step, "", "a skill is never filed as a step");
        assert_eq!(first[0].task, "");
        assert_eq!(first[0].cost_usd, Some(100.0 * 10.0 / 1_000_000.0));

        // Read again with nothing new in the transcript.
        assert!(bank_session_at(&repo, "claude", "s1", &path).is_empty());

        // A new skill, and more of an old one.
        std::fs::write(
            &path,
            transcript(&[("/spoolway-plan", 100), ("", 50), ("/spoolway-queue", 7)]),
        )
        .unwrap();
        let again = bank_session_at(&repo, "claude", "s1", &path);
        let banked: BTreeMap<&str, u64> = again
            .iter()
            .map(|e| (e.skill.as_deref().unwrap(), e.tokens.output))
            .collect();
        assert_eq!(banked["spoolway-plan"], 50, "only the delta");
        assert_eq!(banked["spoolway-queue"], 7);

        // And the ledger totals to the transcript, not to twice it.
        let total: u64 = read(&repo).unwrap().iter().map(|e| e.tokens.output).sum();
        assert_eq!(total, 157);
    }

    /// A ledger line with only the fields a test overrides left to say —
    /// everything else is the quietest value that parses.
    fn plain_entry() -> Entry {
        Entry {
            ts: "2026-08-04T06:14:15+00:00".into(),
            task: String::new(),
            plan: None,
            step: String::new(),
            pipeline: String::new(),
            agent: "claude".into(),
            kind: "claude".into(),
            model: "claude-opus-5".into(),
            session: "s".into(),
            round: 0,
            wall_s: 0,
            turns: 1,
            tokens: Tokens::default(),
            cost_usd: None,
            ctx_peak: None,
            version: None,
            commit: None,
            outcome: None,
            run: None,
            trial: None,
            skill: None,
            project: String::new(),
        }
    }

    /// A line written before skills were labelled banked a whole session under
    /// one label. Re-segmenting it now would count that session twice, so the
    /// session is left exactly as it was banked.
    #[test]
    fn a_session_banked_under_the_old_planning_path_is_left_alone() {
        let (repo, path) = fixture("legacy");
        std::fs::write(&path, transcript(&[("/spoolway-plan", 60), ("/clear", 40)])).unwrap();

        let legacy = Entry {
            ts: chrono::Utc::now().to_rfc3339(),
            task: "auth".into(),
            plan: Some("auth".into()),
            step: "plan".into(),
            agent: INTERACTIVE_AGENT.into(),
            session: "s1".into(),
            turns: 2,
            tokens: Tokens {
                output: 100,
                ..Tokens::default()
            },
            cost_usd: Some(1.0),
            ..plain_entry()
        };
        append(&repo, &legacy).unwrap();

        // It reads back under the planning skill's real name even though
        // nothing wrote the field.
        assert_eq!(read(&repo).unwrap()[0].skill_label(), Some("spoolway-plan"));
        assert!(bank_session_at(&repo, "claude", "s1", &path).is_empty());
        assert_eq!(read(&repo).unwrap().len(), 1);
    }

    /// The stripping `skill_of` wrote `plan` for `/spoolway-plan`. The ledger
    /// is append-only, so those lines still say `plan`; reading them under the
    /// command's real name is what keeps an existing eval table whole.
    #[test]
    fn a_line_banked_under_the_stripped_name_reads_as_the_full_one() {
        let labelled = |skill: &str| Entry {
            ts: chrono::Utc::now().to_rfc3339(),
            agent: INTERACTIVE_AGENT.into(),
            session: "s1".into(),
            skill: Some(skill.to_string()),
            ..plain_entry()
        };

        assert_eq!(labelled("plan").skill_label(), Some("spoolway-plan"));
        // Only that one name is remapped. A project's own skill keeps its own.
        assert_eq!(labelled("my-plan").skill_label(), Some("my-plan"));
    }

    /// Taken by every test that touches a variable the ambient lookup reads —
    /// `CLAUDE_CODE_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_HOME`.
    ///
    /// The environment belongs to the process, not to a test, so two of these
    /// running at once read each other's values and each other's restores —
    /// which is a failure in whichever one happened to look while the other was
    /// putting the variable back, and never in the one at fault. There is no
    /// per-test environment to hand out, so they take turns instead.
    ///
    /// Poison is stepped over deliberately: a test that failed while holding
    /// this has already reported itself, and failing the others as well would
    /// bury it.
    static AMBIENT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A lane is itself a claude session. Without this guard every lane would
    /// bank its own spend a second time, as skill spend.
    #[test]
    fn a_command_inside_a_lane_banks_no_skill_line() {
        let _ambient = AMBIENT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let (repo, path) = fixture("lane");
        std::fs::write(&path, transcript(&[("/spoolway-plan", 10)])).unwrap();

        let previous = std::env::var_os("CLAUDE_CODE_SESSION_ID");
        crate::platform::set_test_env("CLAUDE_CODE_SESSION_ID", "s1");
        crate::platform::set_test_env(crate::dispatch::ENV_STEP, "implement");

        assert!(bank_ambient(&repo).is_empty());
        assert!(sweep(&repo).is_empty());
        assert!(read(&repo).unwrap().is_empty());

        crate::platform::remove_test_env(crate::dispatch::ENV_STEP);
        // Out of a lane, the same session banks — so what the guard turned off
        // is the lane, not the mechanism.
        assert!(!bank_session_at(&repo, "claude", "s1", &path).is_empty());

        match previous {
            Some(value) => crate::platform::set_test_env("CLAUDE_CODE_SESSION_ID", value),
            None => crate::platform::remove_test_env("CLAUDE_CODE_SESSION_ID"),
        }
    }

    /// A lane's line carries no skill, and nothing reads one onto it.
    #[test]
    fn a_lane_line_is_not_a_skill_line() {
        let lane = Entry {
            task: "login".into(),
            step: "review".into(),
            pipeline: "default".into(),
            wall_s: 10,
            ..plain_entry()
        };
        assert_eq!(lane.skill_label(), None);
        assert!(!lane.is_skill());
    }

    #[test]
    #[allow(clippy::items_after_test_module)]
    fn every_ambient_session_is_read_from_the_environment() {
        let _ambient = AMBIENT_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let vars = ["CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID"];
        let previous = vars.map(|var| (var, std::env::var_os(var)));
        for var in vars {
            crate::platform::remove_test_env(var);
        }

        crate::platform::set_test_env("CLAUDE_CODE_SESSION_ID", "abc-123");
        assert_eq!(ambient_sessions(), vec![("claude", "abc-123".to_string())]);

        // A codex session started from inside a claude one: codex inherits the
        // outer variable and exports its own over the top, so both are really
        // in the environment and both sessions are really spending. Read off a
        // real nested run — an `env` dump from inside such a session carried
        // `CLAUDE_CODE_SESSION_ID` *and* `CODEX_THREAD_ID`. Returning only the
        // first would leave the inner session — the one doing the work —
        // permanently unenrolled.
        crate::platform::set_test_env("CODEX_THREAD_ID", "019ffa86-b076");
        assert_eq!(
            ambient_sessions(),
            vec![
                ("claude", "abc-123".to_string()),
                ("codex", "019ffa86-b076".to_string())
            ]
        );

        // Exported but empty is no session, not a session with an empty name.
        crate::platform::set_test_env("CLAUDE_CODE_SESSION_ID", "");
        assert_eq!(
            ambient_sessions(),
            vec![("codex", "019ffa86-b076".to_string())]
        );

        for var in vars {
            crate::platform::remove_test_env(var);
        }
        assert!(ambient_sessions().is_empty());

        for (var, value) in previous {
            if let Some(value) = value {
                crate::platform::set_test_env(var, value);
            }
        }
    }

    #[test]
    fn a_minted_session_id_is_a_v4_uuid() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&id[14..15], "4", "version nibble: {id}");
        assert!(
            matches!(&id[19..20], "8" | "9" | "a" | "b"),
            "variant: {id}"
        );
        assert_ne!(new_session_id(), new_session_id());
    }

    /// Every command banks to the ledger once, and every lane banks again on
    /// top of that once per step — `main.rs:180` and `src/dispatch.rs:1651`
    /// — so several processes appending at once is routine. Real OS threads
    /// exercise the same `open(O_APPEND)` + write syscalls two processes
    /// racing the same ledger would; the atomicity a single `write_all` gets
    /// from append mode is the kernel's, not anything specific to being a
    /// separate process. Before this task's fix, `writeln!` split the line
    /// and its newline into two writes that a second appender could land
    /// between, so every line below has to come back out whole.
    #[test]
    fn two_writers_racing_append_leave_every_line_parseable() {
        let (repo, _) = fixture("append-race");
        let repo = std::sync::Arc::new(repo);

        let writers = 16;
        let handles: Vec<_> = (0..writers)
            .map(|i| {
                let repo = repo.clone();
                std::thread::spawn(move || {
                    let entry = Entry {
                        session: format!("writer-{i}"),
                        ..plain_entry()
                    };
                    append(&repo, &entry).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let raw = std::fs::read_to_string(ledger_path(&repo)).unwrap();
        let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), writers, "a torn write drops or fuses a line");
        for line in &lines {
            serde_json::from_str::<Entry>(line)
                .unwrap_or_else(|e| panic!("line failed to parse: {e}: {line}"));
        }
        let sessions: HashSet<String> = read(&repo)
            .unwrap()
            .into_iter()
            .map(|e| e.session)
            .collect();
        assert_eq!(sessions.len(), writers, "every writer's line must survive");
    }
}
