//! What spoolway knows about each agent CLI a lane can run, in one table.
//!
//! The dispatcher is agent-agnostic on purpose: it starts whatever `kind` a
//! profile names and learns outcomes from the task file, never from the agent.
//! Everything that *is* specific to one CLI — how a profile loads it, which of
//! its credentials a lane has no use for — is a row here. Supporting another
//! agent is adding a row, not touching the dispatcher.
//!
//! A kind is **one** row, spanning both halves of what spoolway knows about it:
//! how it launches, and how its spend is read back. Those used to be two tables
//! in two modules joined by nothing but a matching string, which made a kind
//! that existed in one and not the other an undetectable state rather than a
//! visible one. Now the accounting half is an `Option` on the row that launches
//! it — and `None` is a legal, finished state, not a gap. See [`Accounting`].

use crate::usage::{AbortMarker, FileShape, Format, Store};

/// One agent CLI spoolway knows how to run.
pub struct Adapter {
    /// The kind a profile names and `herdr agent start --kind` receives.
    pub kind: &'static str,

    /// The executable this kind launches, when it is not `kind` itself.
    ///
    /// `None` on every row whose name and binary are the same string — which
    /// is every kind today: `spoolway agent list` and `spoolway agent
    /// verify` resolve the program to look for through this rather than
    /// through `kind` directly, and so does a lane's own argv in
    /// `headless.rs`. Kept as a distinct field rather than folded into `kind`
    /// because a future row may again name a CLI different from its kind.
    pub binary: Option<&'static str>,

    /// How this kind is told whether to stop and ask a person about a tool
    /// call, if it has such a notion at all.
    pub permissions: Option<Permissions>,

    /// How this kind is told how hard to think, if it carries such a flag at
    /// all. `None` on a kind with no such flag — a step's `effort:` is then
    /// simply not rendered into its args.
    pub effort: Option<Effort>,

    /// Whether this kind loads skills at all. `false` on a kind with no notion
    /// of a skill: a step's `skills:` is then refused outright by
    /// `pipeline_check` rather than launched and quietly ignored.
    ///
    /// Only the yes-or-no is kept here. *Where* a kind reads a project's
    /// skills from is [`crate::install::Provider::segments`]'s to say, because
    /// it is the only place that writes them — and no check reads that
    /// directory to decide whether a named skill exists, since a plugin puts
    /// its skills somewhere spoolway cannot enumerate.
    pub skills: bool,

    /// How this kind is driven with no terminal to type into. `None` on a kind
    /// nobody has established this for, whose lanes therefore only run under a
    /// multiplexer — [`crate::mux`] refuses to start them headless rather than
    /// guessing a flag at a real binary.
    pub headless: Option<Headless>,

    /// What makes this kind leave its pane without closing it.
    ///
    /// The same shape as [`Adapter::headless`], and filled in the same way: a
    /// row only where somebody sat down with the real binary and watched it
    /// leave. `None` on every kind nobody has checked, whose panes keep being
    /// closed and re-split exactly as they are today — see
    /// [`crate::mux::Mux::vacate_lane`], which degrades to
    /// [`crate::mux::Mux::stop_lane`] rather than guessing.
    ///
    /// There is no universal way to end an agent, which is why this cannot be
    /// one constant: two Ctrl+C do not end Claude Code and neither does
    /// Ctrl+D at an empty prompt, while `ctrl+d` ends a `pi` outright. A
    /// gesture guessed at the wrong kind is a lane killed mid-turn, or a
    /// stray keystroke typed into somebody's conversation.
    ///
    /// Read by [`crate::mux::Mux::vacate_lane`], which is how a task's pane
    /// carries from one step to the next instead of being closed and split
    /// again.
    pub quit: Option<Quit>,

    /// The argv template a lane of this kind is started with. Placeholders —
    /// `{model}`, `{prompt_file}`, `{task_file}`, `{worktree}`, `{repo}`,
    /// `{state_dir}`, `{project_home}`, `{session_id}` — are substituted by
    /// [`crate::config::AgentProfile::render_args`] from what a lane start
    /// already computes; see that function's doc comment for what each one
    /// carries. Empty on a kind nobody has wired up yet — a profile naming
    /// such a kind is refused rather than launched with no flags at all.
    pub args: &'static [&'static str],

    /// A per-session state directory of spoolway's making, for a kind that
    /// mints its own session id and will not take one. `None` for a kind that
    /// takes the id spoolway minted, which needs none of this.
    pub home: Option<Home>,

    /// How this kind's spend and session size are read back off the transcript
    /// it writes.
    ///
    /// `None` is a finished state, not a gap: the kind launches, runs and lands
    /// its work exactly as a metered one does — it is simply unmetered, and
    /// `spoolway agent list` and `spoolway agent verify` say so on their own
    /// line. Nothing anywhere refuses a kind for being here; see
    /// [`Accounting`] for what an unmetered kind actually loses.
    pub accounting: Option<Accounting>,

    /// The phrase this kind's own CLI leaves in a pane when it has hit a
    /// usage limit and stopped making progress on its own — read verbatim off
    /// a real transcript, not guessed at. A lane whose tail carries this is
    /// not stuck on a question and not dead at launch; it is waiting out a
    /// clock nothing spoolway does will shorten, so the dispatcher leaves the
    /// pane exactly as it is and parks the task instead of reminding it or
    /// tearing it down — see `Dispatcher::usage_limit_hold` in `dispatch.rs`,
    /// checked ahead of the ordinary settled/reminder path rather than inside
    /// it, since this is true of a working pane as much as a settled one.
    ///
    /// `None` for a kind nobody has established this wording for: its lanes
    /// keep going through the ordinary settled/reminder path, exactly as they
    /// did before this field existed.
    pub usage_limit: Option<&'static str>,

    /// Where this kind's own cached usage percentage is read back from.
    /// `spoolway agent verify` prints it verbatim, and [`crate::quota::read`]
    /// decides how to use it from [`Accounting::format`], since the two rows
    /// established so far do not read it the same way.
    ///
    /// **`claude`**: a file relative to the home directory, `.claude.json`,
    /// whose `cachedUsageUtilization.utilization` object carries a `five_hour` and a
    /// `seven_day` entry — each an integer `utilization` percent and an ISO
    /// `resets_at` — plus `fetchedAtMs` on `cachedUsageUtilization`. Read verbatim
    /// off a real file, not guessed at.
    ///
    /// **`codex`**: `"sessions"`, a directory joined onto every per-lane home
    /// spoolway has made for this kind and onto `~/.codex`, then walked for
    /// the newest rollout across all of them. Its last `token_count` event
    /// carries a `rate_limits` object with a `primary` and a `secondary`
    /// window, each a `used_percent` float and an epoch-seconds `resets_at`;
    /// see the comment on codex's own row for the real reading this was
    /// established against, and its negative case.
    ///
    /// `None` for a kind with no such cache to read — every row but these
    /// two today. Enabling a quota ceiling on such a profile holds new
    /// launches; `spoolway agent verify` diagnoses the missing probe.
    pub quota: Option<&'static str>,

    /// How this kind's transcript records a person's interrupt — the record
    /// left behind when Escape lands mid-turn. Read by
    /// [`crate::usage::last_turn_aborted`], which asks whether a session's
    /// transcript *ends* in one; see [`AbortMarker`] for the three shapes
    /// captured off real transcripts.
    ///
    /// `None` for a kind nobody has established this wording for. Inventing
    /// a row for it would be a claim nothing has tested.
    pub abort_marker: Option<AbortMarker>,
}

/// How one agent kind's spend is read back.
///
/// The option wraps the whole half rather than each field: a kind either has a
/// transcript spoolway can find and parse or it does not, and there is no
/// useful state in between. Pricing is deliberately not in here — `[models]`
/// matches on the model name, which is the same name whichever CLI asked for
/// it.
///
/// **What `None` costs.** Nothing breaks; three things degrade, and one
/// consumer does not care:
///
/// - `spoolway eval --by` gets no ledger lines at all for lanes of this kind. This
///   is the one with no fallback — the spend is simply absent.
/// - Session reuse always misses: `dispatch::carried_session` returns
///   `SessionMiss::NotFound` and every step opens a fresh session.
/// - The reminder loop falls back to hashing the pane, which
///   `dispatch::note_progress` already does whenever `usage::last_written`
///   returns nothing. Fine under a multiplexer, weak on the headless backend.
/// - The lane itself is unaffected. It starts, runs, reports and lands.
pub struct Accounting {
    /// Where the transcripts this row reads are rooted, relative to the home
    /// directory. For a kind that takes an id, this is the agent's own session
    /// store, sharded by an escaping of the lane's working directory — which is
    /// exactly what the lookup avoids reproducing. For a [`FileShape::OwnHome`]
    /// kind it is spoolway's own state directory, because the per-session home
    /// underneath it is spoolway's to make; see [`Home`], whose `dir` is then
    /// this same name.
    pub sessions_dir: &'static str,

    /// Where this kind's records live, and how they are enumerated — a file
    /// per session, or rows in a database of the lane's own.
    pub store: Store,

    /// Which record shape those records are written in.
    pub format: Format,

    /// Environment variable an *interactive* session of this agent exports.
    ///
    /// This is what lets spoolway account for work it did not start — planning
    /// and queueing happen in your own session, long before a lane exists, and
    /// the session id arrives in the environment of the `spoolway` process that
    /// session runs. `None` means this agent has no such variable, and its
    /// interactive work simply goes unaccounted.
    pub session_env: Option<&'static str>,
}

/// A state directory of this session's own, for a kind that mints its own
/// session id and will not take one.
///
/// This is the second of the two ways to pin a session, and it exists because
/// codex really does refuse the first. spoolway cannot name codex's session —
/// but it can give that session a *home* of its own, named after the id
/// spoolway minted, and then the session inside it is the only one there.
/// That is what makes `codex exec resume --last` mean *this lane's* session
/// rather than whatever this machine touched most recently, and it is what
/// makes a transcript findable under a name spoolway chose: only which part
/// of the path carries the name moves, from the file to the directory above
/// it.
///
/// Deliberately on the [`Adapter`] rather than on [`Accounting`], where it
/// used to live. Pinning a session and reading its spend are two separate
/// things: a kind can need the first and be incapable of the second, and a
/// home that could only be declared beside an accounting row would have left
/// such a kind resuming whichever session ran last on the machine, which
/// with two lanes up is another lane's.
pub struct Home {
    /// Where these per-session directories live under spoolway's own state
    /// directory. The same name a metered kind's [`Accounting::sessions_dir`]
    /// roots its lookup at — a test in this module holds the two together, so
    /// the home spoolway hands out and the home it reads back cannot drift.
    pub dir: &'static str,

    /// The variable that path is handed to.
    pub env: &'static str,

    /// What is brought over from the agent's real home so a relocated lane is
    /// still logged in.
    ///
    /// Only for a home that moves everything: `CODEX_HOME` takes the
    /// credentials and the provider config with it, and a lane that arrived in
    /// an empty one would be logged out and pointed at nothing. Linked rather
    /// than copied, so a login refreshed in the real home stays good and
    /// nothing spoolway made holds a stale token. Empty for a home that moves
    /// one file, which takes nothing with it.
    ///
    /// One name here is the exception, and it is the one [`Trust::file`]
    /// gives: that file is copied and then written into, because the lane
    /// needs a line in it that the person's own home must not gain.
    pub seed: &'static [&'static str],

    /// How this kind is told, in its own config, that a lane's working
    /// directory may be worked in without asking a person first. `None` for a
    /// kind that never asks.
    pub trust: Option<Trust>,
}

/// The entry that answers an agent's "do you trust this directory?" question
/// before it is asked.
///
/// codex refuses to start in a directory it has not been told about: its
/// interactive frontend opens on `Do you trust the contents of this
/// directory?` and waits for a keypress. A lane's worktree is a path nobody
/// has ever answered for, so every codex lane under a multiplexer stalled on
/// that question with nobody there to press a key.
///
/// Settled by running codex-cli 0.153.2 rather than reasoned about, and three
/// things came out of it. The answer is read from the config file on disk, and
/// only from there — the same table passed as `-c
/// projects."<dir>".trust_level=trusted` is parsed and ignored, so the flag
/// route does not exist. Only the working directory is asked about; the
/// directories granted with `--add-dir` are not. And `codex exec` never asks
/// at all, which is why every headless check spoolway already runs came back
/// green while the lanes a person actually watches did not.
pub struct Trust {
    /// The file the entry is written into, named among [`Home::seed`]. It is
    /// spoolway's own copy of the real one rather than the link the other
    /// seeds get: the entry is about a worktree that exists for one lane, and
    /// writing it through a link would put it in the person's own home.
    pub file: &'static str,

    /// The key that is set, segment by segment rather than as one dotted
    /// path. A segment is a directory, and a directory holds dots — and
    /// backslashes, and on a bad day a quote. `{dir}` is substituted for the
    /// directory being trusted.
    ///
    /// A key rather than a line of text because the file is a person's, and
    /// it may already say something about this directory. `codex exec` writes
    /// a `[projects."<dir>"]` table of its own for every directory it runs
    /// in, so a project root that has ever had a headless turn in it already
    /// has one. Appending a second table with the same name is a *duplicate
    /// key*, which codex reports as `duplicate key` and then refuses to load
    /// the config at all — a lane worse off than the one that only had a
    /// question to answer. Setting the key overwrites whatever was there.
    pub key: &'static [&'static str],

    /// What that key is set to.
    pub value: &'static str,
}

/// How a kind is asked to end its session and hand its pane back.
///
/// One line typed at the agent's own prompt and submitted, which is the only
/// form any kind has needed so far. A kind whose gesture is a keystroke
/// rather than a line is what turns this into an enum, not before — the same
/// reasoning [`Print`] carries about not growing a shape ahead of a second
/// row that needs it.
pub struct Quit {
    /// Typed at the agent's own prompt, then submitted.
    ///
    /// For `claude` this is `/exit`, established by probe against the real
    /// binary: submitted at a settled prompt the agent leaves `herdr agent
    /// list` and the pane it was in survives, at its shell.
    pub line: &'static str,
}

/// How one agent kind runs a turn without a pane.
///
/// Under a multiplexer the prompt is *typed* at a session that stays resident
/// between turns. Headless there is no session to type at: each turn is its own
/// process, and the thing that carries the conversation across them is the
/// session id the profile already pins with `{session_id}`. So a kind is
/// drivable headless exactly when it can (a) take a prompt as an argument and
/// exit, and (b) be pointed back at a session it wrote earlier.
pub struct Headless {
    /// What makes the kind process one prompt and exit, and where it goes.
    pub print: Print,

    /// How a *later* turn re-addresses the session the first one opened.
    pub resume: Resume,
}

/// The tokens that make a kind take one prompt and exit, appended after the
/// lane's own args.
///
/// A slice rather than one flag because it is not always a flag: codex spells
/// it as a subcommand, `exec`, and a subcommand smuggled through as a "flag"
/// would be a row lying about the binary. Every kind spoolway ships takes its
/// print form after its other flags — codex's globals all come *before*
/// `exec`, which is what lets one rendered args row serve both the
/// interactive and headless backends. An earlier row's flags belonged to its
/// own *subcommand* instead, needing the print form prepended rather than
/// appended, which is why this once carried a side. That row is gone, and
/// the next kind that needs the other order is what turns this back into an
/// enum, not before.
pub struct Print(pub &'static [&'static str]);

impl Print {
    pub fn tokens(&self) -> &'static [&'static str] {
        self.0
    }

    /// `args` with the print form appended.
    fn apply(&self, args: Vec<String>) -> Vec<String> {
        args.into_iter()
            .chain(self.0.iter().map(|token| (*token).to_string()))
            .collect()
    }
}

/// What a second turn has to do to the args that started the first one.
///
/// Both kinds pin their session with the same `--session-id {session_id}` a
/// profile already passes; they differ only in whether that flag will accept an
/// id it has already seen. Verified against both binaries rather than read off
/// their help text, because the failure is silent in one direction: a kind that
/// quietly starts a *fresh* session on re-pass would answer a lane's question
/// with none of the context the question came from.
pub enum Resume {
    /// The session flag creates-or-continues, so a later turn is the same argv.
    ///
    /// pi: `--session-id <id>` is documented "creating it if missing", and a
    /// re-pass does continue — it recalled the first turn's answer.
    SameArgs,

    /// The session flag refuses an id it already holds, so a later turn swaps
    /// it for the resume flag.
    ///
    /// claude: a second `--session-id <uuid>` fails with "Session ID … is
    /// already in use"; `--resume <uuid>` continues it and carries the context.
    Swap {
        from: &'static str,
        to: &'static str,
    },

    /// Nothing in the argv names the session at all, so a later turn continues
    /// it with tokens appended after everything else.
    ///
    /// codex mints its own id and refuses any other, so there is no
    /// `{session_id}` in its args to swap. What makes `resume --last`
    /// unambiguous — rather than "whichever session this machine touched
    /// most recently" — is that the lane has a home of its own, holding
    /// exactly one session. The two halves are one mechanism: see [`Home`].
    ///
    /// Settled by running it: `codex exec resume --last` under a per-session
    /// `CODEX_HOME` recalled the first turn's answer and appended to the
    /// *same* rollout rather than opening a second one.
    Tail(&'static [&'static str]),
}

impl Resume {
    /// Rewrite a lane's opening args into the args that continue its session.
    ///
    /// The value is left where it was rather than moved to the end: an agent's
    /// flags are positional to each other often enough that reordering them is
    /// a gamble with no upside.
    pub fn apply(&self, args: &[String]) -> Vec<String> {
        match self {
            // Unchanged: a tail's tokens go after the print form, which these
            // args do not carry yet — appended elsewhere. See [`Resume::tail`]
            // and [`Adapter::headless_args`].
            Resume::SameArgs | Resume::Tail(_) => args.to_vec(),
            Resume::Swap { from, to } => args
                .iter()
                .map(|arg| match arg == from {
                    true => (*to).to_string(),
                    false => arg.clone(),
                })
                .collect(),
        }
    }

    /// What a later turn appends to continue the session, after the print form.
    ///
    /// Empty for a kind that resumes by a flag, because for those the rewrite
    /// already happened in [`Resume::apply`]. The split is about *order*: a
    /// subcommand tail has to land after `exec`, and `exec` is appended by the
    /// driver long after the args are rendered.
    pub fn tail(&self) -> &'static [&'static str] {
        match self {
            Resume::Tail(tokens) => tokens,
            _ => &[],
        }
    }
}

/// How one agent kind spells "do not stop and ask me about that".
///
/// `None` on a kind that has no such setting, which is not the same as one that
/// always asks: pi runs its tools and leaves the gating entirely to its
/// extensions, so there is no mode to pick and nothing for a profile to set.
/// (`pi --no-approve`, which a lane does pass, is about trusting project-local
/// *files* — a different axis, and not this one.)
///
/// A mode is a value a profile names, not a flag it spells out. That is the
/// whole point of the row: `permission_mode = "auto"` means the same thing to
/// every kind that has one, and which flag carries it stays here.
pub struct Permissions {
    /// The flag the mode is passed as.
    pub flag: &'static str,

    /// Every mode this kind accepts.
    ///
    /// The first is what a lane gets when a profile names none, and it is
    /// deliberately the one that lets a lane finish with nobody watching. The
    /// rest are offered because a project may have a reason spoolway does not
    /// know about — a `manual` lane is a strange thing to run a pipeline on,
    /// but refusing to express it would be spoolway deciding, not the project.
    pub modes: &'static [&'static str],
}

impl Permissions {
    /// The mode used when a profile names none.
    pub fn default_mode(&self) -> &'static str {
        self.modes[0]
    }

    pub fn accepts(&self, mode: &str) -> bool {
        self.modes.contains(&mode)
    }
}

/// How one agent kind spells "think this hard".
///
/// Unlike [`Permissions`], there is no closed set of values here: which
/// levels a model accepts is the model's own fact, changes when the model
/// does, and a copy of that list in this binary would only go stale silently
/// — claude already warns and falls back when a level is wrong, and codex
/// passes whatever it is given straight to the provider. This row says one
/// thing only: the argv a step's `effort:` is handed straight through to,
/// verbatim, on a kind that carries the notion at all.
pub struct Effort {
    /// The argv a step's `effort:` renders to, with `{effort}` substituted —
    /// the same templating [`Adapter::args`] uses, and for the same reason.
    ///
    /// A template rather than a flag because the level is not always its own
    /// argv item. claude spells it `--effort <level>`, two tokens; codex has no
    /// flag at all and carries it as a config override, `-c
    /// model_reasoning_effort=<level>`, where the level is embedded *inside*
    /// the second token. A row that could only say "flag, then value" could
    /// describe the first and not the second — which is a limit of the row, not
    /// of the binary, and this is it lifted.
    ///
    /// `{effort}` must appear somewhere, or a step's level would be silently
    /// dropped on the floor; a test in this module holds every row to it.
    pub args: &'static [&'static str],
}

pub const ADAPTERS: &[Adapter] = &[
    Adapter {
        kind: "pi",
        // Its own name is its executable.
        binary: None,
        // pi has no tool-approval prompt to switch off: it runs its tools with
        // nothing gating them. Nothing to set.
        permissions: None,
        // `--thinking` takes a token budget, not a named level — a different
        // axis from what a step's `effort:` means, so it is not wired here.
        effort: None,
        skills: true,
        headless: Some(Headless {
            print: Print(&["--print"]),
            // `--session-id` creates the session if it is missing and continues
            // it if it is not, so a second turn needs no rewriting at all.
            resume: Resume::SameArgs,
        }),
        // `ctrl+d` is known to end a `pi`, but a keystroke is not what this
        // row describes and nobody has driven a pi lane through a handover.
        // Its panes keep being closed and re-split.
        quit: None,
        // `--model` is not optional: pi's default provider is a cloud one, so
        // omitting it silently sends local work to an API. `--no-approve`
        // skips the project-trust dialog a lane would otherwise hang on with
        // no error anywhere. `--no-skills` used to be here too, keeping
        // unrelated global skills out of a small local model's context — it
        // is gone because a step's `skills:` cannot reach pi while it is set,
        // and pi is the only kind whose row promises it can.
        args: &[
            "--model",
            "{model}",
            "--append-system-prompt",
            "{prompt_file}",
            // Pin the session so `spoolway eval --by` can find this lane's
            // transcript by a name spoolway chose rather than by guessing.
            "--session-id",
            "{session_id}",
            "--no-approve",
        ],
        // Takes the id spoolway mints, so it needs no home of its own.
        home: None,
        accounting: Some(Accounting {
            sessions_dir: ".pi/agent/sessions",
            store: Store::Transcript(FileShape::AfterUnderscore),
            format: Format::Pi,
            // Not known to export one. When it does, this is the only line to
            // change for planning done in a pi session to start being counted.
            session_env: None,
        }),
        // A local model has no account-wide usage limit to run out of.
        usage_limit: None,
        // Nor a cached percentage against one.
        quota: None,
        // Captured off a real transcript: the last record after Escape is
        // `{"type":"message","message":{"role":"assistant","content":[],
        // "stopReason":"aborted","errorMessage":"Operation aborted"}}`.
        abort_marker: Some(AbortMarker::PiAborted),
    },
    Adapter {
        kind: "claude",
        // Its own name is its executable.
        binary: None,
        permissions: Some(Permissions {
            flag: "--permission-mode",
            // `auto` first, and so the default: it is the strongest mode that
            // still lets a lane finish alone without stopping at every tool
            // call. `bypassPermissions` is deliberately *not* the default — plenty of
            // organisations disable it outright, and a pipeline that only runs
            // where it is allowed is not one to build on.
            //
            // The rest are here because they are claude's, not because they are
            // sensible to dispatch on: `manual` and `plan` will stop a lane dead
            // waiting for a person, which is a project's business to choose.
            modes: &[
                "auto",
                "acceptEdits",
                "dontAsk",
                "bypassPermissions",
                "plan",
                "manual",
            ],
        }),
        effort: Some(Effort {
            args: &["--effort", "{effort}"],
        }),
        skills: true,
        headless: Some(Headless {
            print: Print(&["--print"]),
            // `--session-id` is create-only: handed an id it already has it
            // refuses outright, which is the good failure. `--resume` takes the
            // same id and continues the conversation.
            resume: Resume::Swap {
                from: "--session-id",
                to: "--resume",
            },
        }),
        // Established by probe against the real binary, not read off any
        // documentation: two Ctrl+C in quick succession do not end Claude
        // Code, and neither does Ctrl+D at an empty prompt. `/exit` submitted
        // at a settled prompt does — the agent leaves `herdr agent list` and
        // its pane is still there, back at its shell.
        //
        // At a *settled* prompt: the same probe sent the gesture to a working
        // Claude Code and got a modal asking what to do about the running
        // task, with the agent sitting in front of it. Waiting for the lane
        // to settle before vacating is the caller's job.
        quit: Some(Quit { line: "/exit" }),
        // `--append-system-prompt-file`, not `--append-system-prompt`: claude
        // takes literal prompt text there, so handing it the path would append
        // the path string and quietly drop the prompt.
        args: &[
            "--model",
            "{model}",
            "--append-system-prompt-file",
            "{prompt_file}",
            "--session-id",
            "{session_id}",
            // A lane's own source is in its worktree, but the task file it
            // works from is not: that lives under the project's own home,
            // outside the checkout entirely. Reading it leaves claude's
            // workspace, and *every* permission mode prompts on that — so
            // without this the first thing a lane does is stop and ask a
            // person who is not there.
            //
            // `--add-dir`, rather than a mode that stops asking, because an
            // enterprise policy may forbid `bypassPermissions` outright and a
            // pipeline that only runs where that is allowed is not one to
            // depend on. `{state_dir}` and `{project_home}` rather than
            // `{repo}` because together they are the whole of what a lane
            // legitimately reads outside its worktree — the task file and
            // its prompts.
            "--add-dir",
            "{state_dir}",
            "--add-dir",
            "{project_home}",
        ],
        // Takes the id spoolway mints, so it needs no home of its own.
        home: None,
        accounting: Some(Accounting {
            sessions_dir: ".claude/projects",
            store: Store::Transcript(FileShape::Exact),
            format: Format::AnthropicApi,
            session_env: Some("CLAUDE_CODE_SESSION_ID"),
        }),
        // Read verbatim off a real transcript — see
        // `~/.spoolway/spoolway/archive/checkout-line.md`'s own `## Blocker`:
        // "Usage limit reached · continuing automatically at 9:50am · esc to
        // cancel", and again as "Usage limit reached again after you
        // continued · continuing automatically…". The phrase itself, not the
        // clock or the rest of the line, is what stays the same between them.
        usage_limit: Some("Usage limit reached"),
        // Read verbatim off a real `~/.claude.json` — see `quota::read`'s own
        // doc for the shape.
        quota: Some(".claude.json"),
        // Captured off a real transcript: the last record after Escape is
        // `{"type":"user","message":{"role":"user","content":
        // [{"type":"text","text":"[Request interrupted by user]"}]}}`.
        abort_marker: Some(AbortMarker::ClaudeInterrupted),
    },
    // Settled against codex-cli 0.147.0, driven against a local
    // OpenAI-compatible endpoint. Every clause below was established by
    // running the binary and reading what it did. See `docs/agents.md` for what
    // was tried and what each run said.
    //
    // An earlier pass left `headless` and `accounting` blank here, on the
    // reasoning that codex mints its own session id and refuses any other, so
    // spoolway had nothing to look a transcript up by. The first half is true
    // and the conclusion was not: a session can be pinned by the *directory* it
    // writes into as well as by the name of the file, and `CODEX_HOME` moves
    // codex's whole state tree. That is what the two rows below rest on, and
    // both were then exercised end to end rather than reasoned about.
    Adapter {
        kind: "codex",
        // Its own name is its executable.
        binary: None,
        permissions: Some(Permissions {
            // The interactive binary's own list, read off its refusal:
            // `codex --ask-for-approval nonsense` prints
            // `[possible values: untrusted, on-request, never]`. `never`
            // first, and so the default — the only one of the three that
            // lets a lane finish with nobody watching.
            //
            // Not on `codex exec` itself, which carries `--approve-for-me`
            // instead — but it does not have to be: every flag in `args` is a
            // *global* one, and codex accepts globals before the subcommand.
            // `codex --ask-for-approval never … exec "<prompt>"` was run and
            // parsed, so the same rendered args serve both backends.
            flag: "--ask-for-approval",
            modes: &["never", "on-request", "untrusted"],
        }),
        // No effort *flag* — codex carries the setting as a config override,
        // with the level embedded inside the argv item rather than following
        // it as its own. That is exactly the shape [`Effort::args`] exists to
        // express.
        //
        // Settled by running it: `-c model_reasoning_effort=high` is accepted
        // under `--strict-config` (so the key is real, not guessed) and lands
        // in the rollout's `turn_context` as `effort: "high"`, which is how the
        // level was confirmed to reach the turn rather than being parsed and
        // dropped. `minimal`, `low` and `medium` all arrive verbatim too — and
        // so does a nonsense level, because codex validates nothing here and
        // hands the value to the provider. That is the same contract claude's
        // row already assumes, and why neither row carries a list of levels.
        effort: Some(Effort {
            args: &["-c", "model_reasoning_effort={effort}"],
        }),
        skills: true,
        // codex mints its own session id and refuses any other: `codex exec
        // resume <a fresh uuid>` fails with "no rollout found for thread id",
        // and `--strict-config` rejects `-c session_id=…` as an unknown field.
        // So there is no id to swap, and `Tail` is the shape that fits — the
        // session is continued by a subcommand, not by naming it.
        //
        // What makes `--last` mean *this lane's* session rather than whatever
        // this machine touched most recently is the `home` below: the lane has
        // a codex home holding exactly one session. Run end to end — a fresh
        // `… exec "<prompt>"` then `… exec resume --last "<prompt>"` in the
        // same home recalled the first turn's answer and appended to the same
        // rollout file, which stayed the only one in the tree.
        headless: Some(Headless {
            // A subcommand, not a flag, and one that may follow the global
            // flags: every flag in `args` is a global, and codex accepts
            // globals before the subcommand — `codex --model m … exec
            // "<prompt>"` was run and parsed.
            print: Print(&["exec"]),
            resume: Resume::Tail(&["resume", "--last"]),
        }),
        // Not driven through a handover. Its panes keep being closed and
        // re-split.
        quit: None,
        // Every flag here is a global one, exercised in real turns against the
        // local endpoint on both forms of the binary: the interactive one a
        // multiplexer lane runs, and `exec` for the headless backend.
        args: &[
            "--model",
            "{model}",
            // codex has no append-a-system-prompt flag. `model_instructions_file`
            // is the only way in, and it *replaces* codex's base instructions
            // rather than appending to them: the same prompt cost 7,503 input
            // tokens without it and 3,055 with it. A turn run this way still
            // used its shell tool and wrote the file it was asked to, so the
            // replacement does not cost the lane its tools.
            "-c",
            "model_instructions_file={prompt_file}",
            // codex sandboxes model-run commands itself, and read-only is its
            // default — a lane that cannot write to its own worktree is not a
            // lane. This is spoolway addressing its own CLI, not a project's
            // to tune, which is why it is here and not a config key.
            "--sandbox",
            "workspace-write",
            // The same reasoning as claude's: a lane's task file lives under
            // the project's own home, outside the worktree codex would
            // otherwise confine it to — `{state_dir}` for its prompts,
            // `{project_home}` for the task file itself.
            "--add-dir",
            "{state_dir}",
            "--add-dir",
            "{project_home}",
            // codex checks for a newer release of itself on startup, and a
            // newer one stops the lane dead: `✨ Update available! 0.153.4 ->
            // 0.153.6`, then `1. Update now (runs npm install -g
            // @openai/codex)`, `2. Skip`, `3. Skip until next version`, and
            // `Press enter to continue`. Nobody is there to press it, so the
            // lane never reaches `working` and the launch is reported failed —
            // and worse if somebody does, because option 1 swaps the binary
            // every *other* live lane is running out from under them. It is
            // the same live-binary replacement hazard spoolway guards against
            // for its own dispatcher, arriving through codex instead.
            //
            // Established by running it: with the check left on and a home
            // whose `version.json` named a newer release, the dialog came up
            // and held the screen; with this override it did not. Unlike the
            // trust table beside it, this key really is read from the merged
            // config, so it is a flag here rather than something written into
            // the lane's file. It is accepted under `--strict-config`, and a
            // near miss — `check_for_update_on_startupp` — is refused, so the
            // name is the binary's and not a guess.
            //
            // Only the lane's own copy of codex stops asking. A person's own
            // codex is a different process reading a config spoolway never
            // touched, and still tells them.
            "-c",
            "check_for_update_on_startup=false",
        ],
        // `CODEX_HOME` relocates the whole state tree, so the lane gets a codex
        // home of its own named after the id spoolway minted — which is what
        // makes `resume --last` this lane's session, and the rollout inside it
        // the lane's transcript however codex chose to name the file. Verified:
        // with it pointed at a per-session directory a turn wrote its rollout
        // there and nothing landed under `~/.codex` at all.
        home: Some(Home {
            dir: "codex",
            env: "CODEX_HOME",
            // Which means the credentials and the provider config move too, and
            // a lane that arrived in an empty home would be logged out and
            // pointed at nothing.
            seed: &["config.toml", "auth.json"],
            // See [`Trust`] for what was run to establish this, and why the
            // route is a file rather than a flag.
            trust: Some(Trust {
                file: "config.toml",
                key: &["projects", "{dir}", "trust_level"],
                value: "trusted",
            }),
        }),
        // codex records per-turn usage as an `event_msg` of type `token_count`,
        // one per `task_started`, under
        // `<CODEX_HOME>/sessions/<yyyy>/<mm>/<dd>/rollout-<ts>-<id>.jsonl`.
        //
        // The id in that filename is codex's, not spoolway's, so the filename
        // is not what identifies the session — the directory is. `CODEX_HOME`
        // relocates the whole tree, and a lane gets one named after the id
        // spoolway minted, so the rollout inside it is unambiguous however
        // codex chose to name it. Verified: with `CODEX_HOME` pointed at a
        // per-session directory, a turn wrote its rollout there and nothing
        // landed under `~/.codex` at all.
        accounting: Some(Accounting {
            sessions_dir: "codex",
            // codex keeps a plugin fixture `.jsonl` under `.tmp` in the same
            // home, so the walk is rooted at `sessions` rather than at the home
            // — see [`FileShape::OwnHome`].
            store: Store::Transcript(FileShape::OwnHome { under: "sessions" }),
            format: Format::Codex,
            // codex exports the id of the session it is running into every
            // command that session runs, under a name of its own: not
            // `SESSION`, which is why an earlier pass looked for one and
            // concluded there was none. Read off a real turn rather than a
            // help string — `env` run from inside a session printed
            // `CODEX_THREAD_ID=019ffa86-…`, and that is exactly the id in the
            // rollout's filename and its `session_meta`. Both frontends do it:
            // the same reading came back with `originator: codex-tui` and with
            // `codex_exec`.
            //
            // So the interactive half is accounted for after all, and it is
            // pinned the *other* way round from a lane's — codex named this
            // one, in a home spoolway never made. See [`FileShape::OwnHome`],
            // which resolves both.
            session_env: Some("CODEX_THREAD_ID"),
        }),
        // Not established against a real codex transcript yet.
        usage_limit: None,
        // Read verbatim off a real ChatGPT-authed rollout on this machine —
        // `~/.codex/sessions/2026/09/05/rollout-2026-09-05T09-51-18-
        // 01a0708c-ec8f-7a01-b4f9-99f6337e1a05.jsonl`'s last `token_count`
        // event carried:
        //   "rate_limits":{"limit_id":"codex","limit_name":null,
        //   "primary":{"used_percent":5.0,"window_minutes":300,
        //   "resets_at":1788611977},"secondary":{"used_percent":2.0,
        //   "window_minutes":10080,"resets_at":1789151593},
        //   "credits":{"has_credits":false,"unlimited":false,"balance":"0"},
        //   "individual_limit":null,"spend_control_reached":null,
        //   "plan_type":"plus","rate_limit_reached_type":null}
        // — a non-null `primary`, only ever seen after a `chatgpt`
        // `auth_mode` sign-in; a local endpoint or an API key leave
        // `rate_limits` present but `primary` and `secondary` null, along
        // with every other field but `limit_id` itself (which stayed
        // `"codex"`) — see [`crate::quota`]'s own doc — which is the
        // negative case its reader has to return no reading for.
        //
        // `"sessions"` rather than a file: unlike claude's single cache file,
        // codex writes this per session, so [`crate::quota::read`] walks for
        // the newest rollout across every per-lane home spoolway has made
        // under its own state directory *and* `~/.codex`, joining this onto
        // each one, and reads the last `token_count` event's `rate_limits`
        // out of whichever file that is.
        //
        // `~/.codex` was excluded once, on the grounds that a session
        // spoolway did not start should not override a lane it did. That
        // deadlocked the queue: only a codex lane writes a managed rollout,
        // and the gate reading it holds every codex lane, so a reading that
        // aged out could never be replaced. The worry it was guarding
        // against — a run settled against a local endpoint — writes both
        // windows null, and [`crate::quota`] skips those rather than letting
        // them win on recency, which is where that guard belongs.
        quota: Some("sessions"),
        // Captured off a real rollout: the record after Escape is a `user`
        // item whose text opens `<turn_aborted>` —
        // `{"role":"user","content":[{"type":"input_text","text":
        // "<turn_aborted>\nThe user interrupted the previous turn on
        // purpose…"}]}`, wrapped as every rollout record is.
        abort_marker: Some(AbortMarker::CodexTurnAborted),
    },
];

/// The row for one kind, if spoolway knows it.
pub fn adapter(kind: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|adapter| adapter.kind == kind)
}

impl Adapter {
    /// The argv that reopens a lane's session interactively, in a terminal a
    /// person types into — what `spoolway lane --attach` runs.
    ///
    /// Derived from the same headless row that carries the resume semantics:
    /// a kind whose session flag creates-or-continues is reopened with it, and
    /// a kind that refuses a seen id is reopened with its resume flag. `None`
    /// on a kind with no established row, for the same reason headless refuses
    /// it — a guessed flag against a real binary loses a session silently.
    /// Whether this kind can be pointed back at a session it wrote earlier.
    ///
    /// The same row headless reads, asked before there are any args to rewrite:
    /// a lane's session id has to be settled before its argv is rendered, and
    /// that decision is only safe for a kind whose resume semantics were
    /// established against the real binary.
    pub fn resumes(&self) -> bool {
        self.headless.is_some()
    }

    /// Whether `tail` — a lane's own pane, its last output — is this kind's
    /// own usage-limit message.
    ///
    /// The dispatcher asks this rather than matching a pattern of its own:
    /// the exact wording is a fact about one CLI, established by reading a
    /// real transcript, and belongs on the row for that CLI rather than
    /// duplicated — or drifting — in `dispatch.rs`. `false` on a kind with no
    /// [`Adapter::usage_limit`] established, which leaves its lanes on the
    /// ordinary settled/reminder path.
    pub fn is_usage_limit(&self, tail: &str) -> bool {
        self.usage_limit.is_some_and(|phrase| tail.contains(phrase))
    }

    /// Whether spoolway knows how to start this kind at all.
    ///
    /// The one clause of the contract that really is a refusal: a row with no
    /// `args` cannot be launched, because there is nothing to launch it with.
    /// Every other clause degrades.
    pub fn launches(&self) -> bool {
        !self.args.is_empty()
    }

    /// The program a lane of this kind actually runs.
    ///
    /// `kind`, unless [`Adapter::binary`] names something else. Every caller
    /// that used to spawn or resolve `kind` directly — `headless.rs`'s argv,
    /// `agent list` and `agent verify`'s `which` — goes through this instead,
    /// so a kind's name and its executable can drift apart in exactly one
    /// place.
    pub fn program(&self) -> &'static str {
        self.binary.unwrap_or(self.kind)
    }

    /// Whether this kind's spend can be read back — see [`Accounting`] for
    /// what the answer costs when it is `false`.
    pub fn meters(&self) -> bool {
        self.accounting.is_some()
    }

    /// A lane's opening argv, rewritten to continue the session it names rather
    /// than open a new one.
    ///
    /// The rewrite belongs to the kind, not to the driver: a lane resumed under
    /// a multiplexer is a second turn that happens to be given its own pane, and
    /// the flag it needs is the one a second headless turn would use. `None` on
    /// a kind with no established row, for the same reason headless refuses it.
    pub fn resume_args(&self, args: &[String]) -> Option<Vec<String>> {
        let headless = self.headless.as_ref()?;
        Some(with_tail(
            headless.resume.apply(args),
            headless.resume.tail(),
        ))
    }

    /// The argv for one headless turn, composed from a lane's stored opening
    /// args.
    ///
    /// The composition, and not just the pieces, is this row's business —
    /// because the order differs by kind. A flag-resuming kind is rewritten in
    /// place and then has its print flag appended; codex has to have `exec` land
    /// *before* `resume --last`, which no amount of rewriting the args alone can
    /// arrange. See [`Print`].
    ///
    /// `stored` may already carry the tail — a lane launched as a resume is
    /// stored with the args it was launched with — so it is stripped first and
    /// re-added under this call's own answer. That makes the whole thing
    /// idempotent, which is what the flag-resuming kinds get for free from
    /// [`Resume::Swap`] and this kind has to be given.
    pub fn headless_args(&self, stored: &[String], later_turn: bool) -> Option<Vec<String>> {
        let headless = self.headless.as_ref()?;
        let tail = headless.resume.tail();
        let (base, carried) = without_tail(stored, tail);
        let resuming = later_turn || carried;

        let args = match resuming {
            true => headless.resume.apply(&base),
            false => base,
        };
        let args = headless.print.apply(args);
        Some(match resuming {
            true => with_tail(args, tail),
            false => args,
        })
    }

    pub fn attach_args(&self, session: &str) -> Option<Vec<String>> {
        let headless = self.headless.as_ref()?;
        let mut argv = vec![self.kind.to_string()];
        match &headless.resume {
            // The session is named, so reopening it is naming it again.
            Resume::SameArgs => argv.extend(["--session-id".to_string(), session.to_string()]),
            Resume::Swap { to, .. } => argv.extend([(*to).to_string(), session.to_string()]),
            // Nothing to name. What makes this the lane's session and not some
            // other is the home the caller sets — see [`session_home`], which
            // is why `attach` sets it before running this.
            Resume::Tail(tokens) => argv.extend(tokens.iter().map(|token| (*token).to_string())),
        }
        Some(argv)
    }

    /// The environment a lane of this kind needs for the session it is pinned
    /// to.
    ///
    /// One entry, and only for a kind that pins by directory rather than by
    /// id: the home spoolway made for this session. Empty for every other
    /// kind, which is why callers can apply it unconditionally.
    ///
    /// Computed rather than templated, which is why it is not in
    /// [`Adapter::args`]: the path is spoolway's own state directory and this
    /// session's id, neither of which a row could spell.
    pub fn session_env(&self, session: &str) -> Vec<(String, String)> {
        let Some(home) = self.home.as_ref() else {
            return Vec::new();
        };
        match session_home(self.kind, session) {
            Some(dir) => vec![(home.env.to_string(), dir.display().to_string())],
            None => Vec::new(),
        }
    }
}

/// `args` without a trailing resume tail, and whether it had one.
fn without_tail(args: &[String], tail: &[&'static str]) -> (Vec<String>, bool) {
    if tail.is_empty() || args.len() < tail.len() {
        return (args.to_vec(), false);
    }
    let split = args.len() - tail.len();
    match args[split..]
        .iter()
        .zip(tail)
        .all(|(arg, token)| arg == token)
    {
        true => (args[..split].to_vec(), true),
        false => (args.to_vec(), false),
    }
}

/// `args` with the resume tail appended, unless it is already there.
fn with_tail(mut args: Vec<String>, tail: &[&'static str]) -> Vec<String> {
    if tail.is_empty() || without_tail(&args, tail).1 {
        return args;
    }
    args.extend(tail.iter().map(|token| (*token).to_string()));
    args
}

/// The home spoolway keeps for one session of a kind that mints its own id.
///
/// Under spoolway's own state directory rather than the agent's, because it is
/// spoolway's directory: the agent is told to use it, and would not have chosen
/// it. The session id is the leaf, which is the whole mechanism — it is what
/// makes the transcript inside findable by a name spoolway chose, and it is the
/// same path [`crate::usage`] resolves when it goes looking. `None` for a kind
/// that takes an id directly, or when there is no home directory to root it in.
pub fn session_home(kind: &str, session: &str) -> Option<std::path::PathBuf> {
    let home = adapter(kind)?.home.as_ref()?;
    Some(crate::usage::state_root()?.join(home.dir).join(session))
}

/// Make the per-session home, seeded so the agent still knows who it is.
///
/// Relocating a whole home — `CODEX_HOME` — moves the credentials and the
/// provider config along with the transcripts, which would leave a lane logged
/// out and pointed at nothing. So the files named by [`Home::seed`] are linked
/// back to the real home rather than copied: a login refreshed there stays good
/// here, and nothing spoolway made holds a stale copy of a token. A home that
/// relocates one file seeds nothing, because it moved nothing away.
///
/// `cwd` is the directory the lane will be started in — its worktree. A kind
/// with a [`Trust`] row is told in its own config that this directory is one it
/// may work in, which is what keeps it from opening on a question nobody is
/// there to answer.
///
/// Best-effort, and deliberately not a failure: the worst case is a lane that
/// cannot authenticate, which is the lane's own error to report, and it is a
/// worse outcome to refuse to start one over a symlink.
pub fn prepare_session_home(
    kind: &str,
    session: &str,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let home = session_home(kind, session)?;
    std::fs::create_dir_all(&home).ok()?;

    let relocates = adapter(kind).and_then(|a| a.home.as_ref());
    let trust = relocates.and_then(|h| h.trust.as_ref());
    let seed = relocates.map(|h| h.seed);
    // Only what a lane cannot run without. Everything else in an agent's home
    // is state it will rebuild on its own, and linking it would be sharing
    // mutable state between concurrent lanes.
    let real = own_home(kind);
    if let (Some(seed), Some(real)) = (seed.filter(|s| !s.is_empty()), real.as_deref()) {
        for name in seed {
            // The trust file is written below rather than linked, so the entry
            // lands in this lane's copy and not in the person's own home.
            if trust.is_some_and(|t| t.file == *name) {
                continue;
            }
            let (from, to) = (real.join(name), home.join(name));
            if from.exists() && !to.exists() {
                link_seed(&from, &to);
            }
        }
    }
    if let Some(trust) = trust {
        write_trust(trust, real.as_deref(), &home, cwd);
    }
    Some(home)
}

/// Write the lane's copy of the kind's config, with `cwd` trusted in it.
///
/// The person's own file first, so the lane keeps the provider config and
/// everything else they set, then the one key set on top of it. Edited as TOML
/// rather than appended to as text, for two reasons. The file may already say
/// something about this directory — `codex exec` writes a `projects` table for
/// every directory it runs in — and a second table with the same name is a
/// duplicate key that costs the lane the whole config. And a directory is not
/// a safe thing to splice into a line: it holds dots, backslashes, and on
/// Windows both at once.
///
/// A real file that does not parse is passed through untouched. The lane then
/// gets the agent's own complaint about the person's config, which is the one
/// they need to see, rather than a truncated config spoolway invented and a
/// lane quietly running against no provider at all.
///
/// Best-effort, the same as the links beside it. A lane whose config could not
/// be written asks its question and stalls, which is the state this is fixing,
/// not a worse one.
fn write_trust(
    trust: &Trust,
    real: Option<&std::path::Path>,
    home: &std::path::Path,
    cwd: &std::path::Path,
) {
    let to = home.join(trust.file);
    if to.exists() {
        return;
    }
    let text = real
        .map(|real| real.join(trust.file))
        .and_then(|from| std::fs::read_to_string(from).ok())
        .unwrap_or_default();
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        let _ = std::fs::write(to, text);
        return;
    };
    if set_key(&mut doc, trust, cwd).is_some() {
        let _ = std::fs::write(to, doc.to_string());
    }
}

/// Set `trust.key` in `doc`, making the tables on the way down.
///
/// `None` when a segment names something that is already there and is not a
/// table — a person who wrote `projects = "yes"` has a config spoolway has no
/// business rearranging, and the caller leaves their file alone.
fn set_key(doc: &mut toml_edit::DocumentMut, trust: &Trust, cwd: &std::path::Path) -> Option<()> {
    let dir = cwd.display().to_string();
    let (last, tables) = trust.key.split_last()?;
    let mut at = doc.as_table_mut();
    for segment in tables {
        let segment = segment.replace("{dir}", &dir);
        at = at
            .entry(&segment)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()?;
    }
    at[&last.replace("{dir}", &dir)] = toml_edit::value(trust.value);
    Some(())
}

/// Link `from` to `to`, and copy only where the platform will not make a
/// link at all.
///
/// A link keeps a credential refreshed in the real home valid here; a copy
/// goes stale the moment the token rotates, and one is left per lane (review
/// finding 63). Unix has always had `symlink`. On Windows a symlink needs a
/// privilege a service often lacks, so a hard link — same volume — is the
/// fallback before a copy; on any other platform a hard link is all that is
/// tried before copying.
fn link_seed(from: &std::path::Path, to: &std::path::Path) {
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(from, to).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(from, to)
        .or_else(|_| std::fs::hard_link(from, to))
        .is_ok();
    #[cfg(not(any(unix, windows)))]
    let linked = std::fs::hard_link(from, to).is_ok();

    if !linked {
        let _ = std::fs::copy(from, to);
    }
}

/// The home this agent keeps for *itself* — the one spoolway relocates away
/// from, never one of the per-session homes it makes.
///
/// Two callers, and they want it for opposite reasons: [`prepare_session_home`]
/// reads it to link a lane's credentials back to the real login, and
/// [`crate::usage`] reads it to find the transcript of a session spoolway never
/// started — an interactive one, which is written here rather than under a home
/// spoolway named. One resolver rather than two, because the two agreeing is
/// the whole point: a lane seeded from one directory and read back from another
/// would be an accounting hole with nothing anywhere saying so.
///
/// `$CODEX_HOME` when it is set, `~/.codex` otherwise — codex's own rule, and
/// the variable is the row's rather than a name spelled here twice. Only for
/// a kind whose [`Home`] relocates a whole tree, which every remaining row
/// with one does. Every kind with no `Home` at all keeps no home spoolway has
/// any business resolving.
pub fn own_home(kind: &str) -> Option<std::path::PathBuf> {
    own_home_in(&crate::platform::home_dir()?, kind)
}

/// [`own_home`] against an explicit home directory, so a test can ask for one
/// that is not the machine's.
pub fn own_home_in(home: &std::path::Path, kind: &str) -> Option<std::path::PathBuf> {
    let relocates = adapter(kind)?.home.as_ref()?;
    let var = relocates.env;
    Some(match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        // The dotfile convention every one of these CLIs follows, and codex
        // really does: with `CODEX_HOME` unset, a turn wrote its rollout under
        // `~/.codex/sessions`.
        _ => home.join(format!(".{kind}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_appears_once() {
        let mut kinds: Vec<&str> = ADAPTERS.iter().map(|a| a.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), ADAPTERS.len());
        for row in ADAPTERS {
            assert!(
                adapter(row.kind).is_some(),
                "`{}` is in the table but not findable by the name a profile declares",
                row.kind
            );
        }
    }

    /// A pane can only be handed back at a shell prompt for a kind whose quit
    /// gesture has actually been checked against the real binary — every other
    /// kind has to keep today's close-and-resplit teardown rather than have
    /// one guessed at it. `claude` is the only kind the probe covered: two
    /// Ctrl+C do not end it, Ctrl+D at an empty prompt does nothing, and
    /// `/exit` submitted at a settled prompt does — see the plan this task
    /// comes from.
    #[test]
    fn only_claude_carries_a_quit_gesture() {
        for row in ADAPTERS {
            if row.kind == "claude" {
                let quit = row.quit.as_ref().unwrap_or_else(|| {
                    panic!("`claude` must carry the quit gesture the probe found")
                });
                assert_eq!(quit.line, "/exit");
            } else {
                assert!(
                    row.quit.is_none(),
                    "`{}` claims a quit gesture nobody has checked against its binary",
                    row.kind
                );
            }
        }
    }

    /// A kind's own cache format is established the same way its quit gesture
    /// is — against a real file, not guessed at — so only a kind whose
    /// reading has actually been read off a real run may claim a probe.
    #[test]
    fn only_claude_and_codex_carry_a_quota_probe() {
        for row in ADAPTERS {
            match row.kind {
                "claude" => assert_eq!(row.quota, Some(".claude.json")),
                "codex" => assert_eq!(row.quota, Some("sessions")),
                _ => assert!(
                    row.quota.is_none(),
                    "`{}` claims a quota probe nobody has read off its real cache",
                    row.kind
                ),
            }
        }
    }

    /// The accounting half used to be a second table in `usage`, joined to this
    /// one by nothing but a matching string — so a kind could be launchable and
    /// unaccountable with nothing anywhere saying so. It is one row now, and
    /// this is the assertion that keeps it one: what `usage::kind()` resolves
    /// has to be the very row `ADAPTERS` carries, for the two kinds that were
    /// settled against real binaries.
    #[test]
    fn usage_resolves_accounting_through_the_adapter_table() {
        for kind in ["pi", "claude", "codex"] {
            let declared = adapter(kind)
                .and_then(|a| a.accounting.as_ref())
                .unwrap_or_else(|| panic!("`{kind}` must carry an accounting row"));
            let resolved = crate::usage::kind(kind)
                .unwrap_or_else(|| panic!("`usage::kind` must resolve `{kind}`"));
            assert!(
                std::ptr::eq(declared, resolved),
                "`usage::kind(\"{kind}\")` resolved something other than the adapter's own row"
            );
        }

        // And the other direction: a kind with no accounting row resolves to
        // nothing rather than to some other kind's transcript shape.
        //
        // Every shipped kind is metered now, so there is nothing left to hold
        // this state up with. The state itself is still real, and the next
        // kind spoolway ships unprobed will need this assertion again.
        assert!(crate::usage::kind("nosuchkind").is_none());
    }

    /// The two halves of pinning-by-home have to agree, or the mechanism is
    /// only half there.
    ///
    /// A [`FileShape::OwnHome`] transcript, read out of a home spoolway made,
    /// on a kind with no [`Home`] would look in a home the agent was never
    /// told to use, so every reading would come back empty with nothing
    /// saying why. The converse is *not* an error and is why `home` is not on
    /// the accounting row: a kind can need its session pinned — codex without
    /// a home would continue whichever session ran last on this machine —
    /// and still have nothing to read back.
    #[test]
    fn pinning_by_home_is_declared_wherever_a_lookup_relies_on_it() {
        for row in ADAPTERS {
            if let Some(accounting) = row.accounting.as_ref() {
                let needs_home = matches!(
                    accounting.store,
                    crate::usage::Store::Transcript(crate::usage::FileShape::OwnHome { .. })
                );
                if needs_home {
                    let home = row.home.as_ref().unwrap_or_else(|| {
                        panic!(
                            "`{}` reads its spend out of a per-session home but declares none",
                            row.kind
                        )
                    });
                    // The lookup roots itself at the accounting row's directory
                    // and the launch hands out the `Home`'s. Two spellings of
                    // one directory that could drift apart silently — a lane
                    // writing to one and every reading looking in the other.
                    assert_eq!(
                        home.dir, accounting.sessions_dir,
                        "`{}` hands out a home under one name and reads it back under another",
                        row.kind
                    );
                }
            }

            let Some(home) = row.home.as_ref() else {
                continue;
            };
            // The id has to reach the path, or two lanes of one kind would
            // share a home — which for a `Tail` kind means resuming each
            // other's conversation.
            let dir = session_home(row.kind, "a-session-id");
            assert!(
                dir.is_some_and(|dir| dir.ends_with("a-session-id")),
                "`{}` pins by home but the session is not what names it",
                row.kind
            );
            // And what the agent is actually handed has to be that path, under
            // the variable the row names — the whole directory, since every
            // remaining `Home` relocates a whole tree rather than one file
            // inside it.
            let env = adapter(row.kind).unwrap().session_env("a-session-id");
            assert_eq!(env.len(), 1, "`{}` hands out no home", row.kind);
            assert_eq!(env[0].0, home.env);
            assert!(
                env[0].1.ends_with("a-session-id"),
                "`{}` hands out `{}`, which does not name the session",
                row.kind,
                env[0].1
            );
        }
    }

    /// A resume that names no session is only this lane's session because the
    /// home is. The two arrive from different clauses of the row, and a kind
    /// that grew one without the other would quietly resume whatever ran last
    /// on the machine — which, with two lanes up, is the other lane.
    #[test]
    fn a_kind_that_resumes_without_naming_a_session_pins_one_by_home() {
        for row in ADAPTERS {
            let Some(headless) = row.headless.as_ref() else {
                continue;
            };
            if matches!(headless.resume, Resume::Tail(_)) {
                assert!(
                    row.home.is_some(),
                    "`{}` continues a session it cannot name and pins none, so it would \
                     resume another lane's",
                    row.kind
                );
            }
        }
    }

    /// Every row says whether it loads skills, named explicitly rather than
    /// derived — so a kind added to [`ADAPTERS`] without an opinion on
    /// `skills` here fails this test instead of silently inheriting `false`
    /// and having every `skills:` step refused against it.
    ///
    /// Whether it carries an [`AbortMarker`] rides the same guard: a kind
    /// added without an opinion here fails this test instead of silently
    /// inheriting `None` and having its interrupts go unnoticed forever.
    #[test]
    fn every_kind_declares_whether_it_loads_skills() {
        let expected: &[(&str, bool, bool)] = &[
            ("pi", true, true),
            ("claude", true, true),
            ("codex", true, true),
        ];
        assert_eq!(
            expected.len(),
            ADAPTERS.len(),
            "a kind was added to ADAPTERS without a matching entry here"
        );
        for (kind, skills, has_abort_marker) in expected {
            let row = adapter(kind).unwrap();
            assert_eq!(
                row.skills, *skills,
                "`{kind}` disagrees on whether it loads skills"
            );
            assert_eq!(
                row.abort_marker.is_some(),
                *has_abort_marker,
                "`{kind}` disagrees on whether it carries an abort marker"
            );
        }
    }

    /// A step's `effort:` has to actually reach the argv. A template that lost
    /// its placeholder would render a flag with no level behind it — the step
    /// would look configured, the lane would run at the model's default, and
    /// nothing anywhere would say the level had been dropped.
    #[test]
    fn every_effort_row_carries_the_level_it_is_given() {
        for row in ADAPTERS {
            let Some(effort) = row.effort.as_ref() else {
                continue;
            };
            assert!(
                effort.args.iter().any(|arg| arg.contains("{effort}")),
                "`{}` has an effort row that would drop the level it is given",
                row.kind
            );
            // And the level survives rendering, wherever in the item it sits —
            // its own argv item for one kind, inside a `key=value` for another.
            let rendered =
                crate::config::AgentProfile::for_kind(row.kind).effort_args(Some("high"));
            assert!(
                rendered.iter().any(|arg| arg.contains("high")),
                "`{}` renders an effort level that does not carry it: {rendered:?}",
                row.kind
            );
            assert!(
                !rendered.iter().any(|arg| arg.contains("{effort}")),
                "`{}` left its placeholder unsubstituted: {rendered:?}",
                row.kind
            );
        }
    }

    /// codex resumes by a subcommand appended *after* the print form, which is
    /// an order no rewrite of the args alone can produce — so the composition
    /// is what gets asserted, on both backends, rather than the pieces.
    #[test]
    fn a_codex_lane_resumes_by_a_subcommand_after_the_print_form() {
        let codex = adapter("codex").unwrap();
        let opening: Vec<String> = ["--model", "m", "--sandbox", "workspace-write"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let first = codex.headless_args(&opening, false).unwrap();
        assert_eq!(
            first,
            ["--model", "m", "--sandbox", "workspace-write", "exec"]
        );

        let later = codex.headless_args(&opening, true).unwrap();
        assert_eq!(
            later,
            [
                "--model",
                "m",
                "--sandbox",
                "workspace-write",
                "exec",
                "resume",
                "--last"
            ],
            "`resume --last` must land after `exec`, not before it"
        );

        // A lane launched as a resume is stored with the tail already on it,
        // and the driver rewrites again on every later turn. Appending twice
        // would hand codex `resume --last resume --last`, so this has to be
        // idempotent the way the flag-swapping kinds are for free.
        let carried = codex.resume_args(&opening).unwrap();
        assert_eq!(carried.iter().filter(|arg| *arg == "resume").count(), 1);
        assert_eq!(
            codex.headless_args(&carried, true).unwrap(),
            later,
            "composing from args that already carry the tail must not double it"
        );
        assert_eq!(
            codex.resume_args(&carried).unwrap(),
            carried,
            "the interactive rewrite must be idempotent too"
        );
    }

    /// Under a multiplexer there is no print form, so the tail goes straight
    /// after the flags — and it still has to be there, or a resumed pane opens
    /// a fresh session while claiming to continue one.
    #[test]
    fn a_resumed_codex_pane_is_not_the_same_argv_as_a_fresh_one() {
        let codex = adapter("codex").unwrap();
        let opening: Vec<String> = ["--model", "m"].iter().map(|s| s.to_string()).collect();
        let resumed = codex.resume_args(&opening).unwrap();

        assert_ne!(resumed, opening, "an unchanged argv opens a fresh session");
        assert_eq!(resumed, ["--model", "m", "resume", "--last"]);
    }

    /// The two flag-resuming kinds must not have grown a tail: their rewrite
    /// happens in place, and an appended `resume` would be a second, contrary
    /// instruction on the same argv.
    #[test]
    fn a_kind_that_resumes_by_a_flag_appends_nothing() {
        for kind in ["pi", "claude"] {
            let headless = adapter(kind).unwrap().headless.as_ref().unwrap();
            assert!(
                headless.resume.tail().is_empty(),
                "`{kind}` resumes by a flag and must append nothing"
            );
        }
    }

    /// The claude row exists because `--session-id` refuses a second use. If
    /// the swap ever stops being applied, a lane's second turn opens an empty
    /// session and answers a question it cannot see — so assert the rewrite
    /// itself, on args shaped like the ones a profile actually renders.
    #[test]
    fn a_claude_lane_resumes_by_swapping_the_session_flag() {
        let headless = adapter("claude").unwrap().headless.as_ref().unwrap();
        let opening: Vec<String> = ["--model", "m", "--session-id", "abc", "--add-dir", "/s"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let resumed = headless.resume.apply(&opening);

        assert_eq!(
            resumed,
            ["--model", "m", "--resume", "abc", "--add-dir", "/s"],
            "the session id must survive in place, with only its flag changed"
        );
    }

    /// pi's flag creates-or-continues, so rewriting anything would be inventing
    /// a difference the binary does not have.
    #[test]
    fn a_pi_lane_resumes_on_the_args_it_started_with() {
        let headless = adapter("pi").unwrap().headless.as_ref().unwrap();
        let opening: Vec<String> = ["--session-id", "abc", "--no-approve"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(headless.resume.apply(&opening), opening);
    }

    /// A resume that finds no session flag to swap would run a fresh session
    /// while claiming to continue one — the exact silent context loss the row
    /// exists to prevent. Nothing in the codebase may reach that state, because
    /// `config.rs` validates every profile passes `{session_id}`; this pins the
    /// rewrite's own half of that contract.
    #[test]
    fn a_swap_that_matches_nothing_changes_nothing() {
        let resume = Resume::Swap {
            from: "--session-id",
            to: "--resume",
        };
        let args: Vec<String> = ["--model", "m"].iter().map(|s| s.to_string()).collect();
        assert_eq!(resume.apply(&args), args);
    }

    /// A kind that has to be told a directory is trusted is told in a file it
    /// also seeds, or the entry would be written into a home the lane never
    /// reads.
    #[test]
    fn a_trust_file_is_one_of_the_seeds() {
        for adapter in ADAPTERS {
            let Some(home) = adapter.home.as_ref() else {
                continue;
            };
            let Some(trust) = home.trust.as_ref() else {
                continue;
            };
            assert!(
                home.seed.contains(&trust.file),
                "`{}` trusts through `{}`, which it does not seed",
                adapter.kind,
                trust.file
            );
            assert!(
                trust.key.iter().any(|segment| segment.contains("{dir}")),
                "`{}`'s trust key names no directory: {:?}",
                adapter.kind,
                trust.key
            );
        }
    }

    /// The lane's copy carries what the person configured *and* the entry, and
    /// is a real file — writing through a link would put a lane's worktree in
    /// the person's own config, which the next lane would inherit along with
    /// every worktree before it.
    #[test]
    fn trust_is_written_into_the_lanes_own_copy() {
        let real = crate::scratch::root("agent-trust-real");
        std::fs::create_dir_all(&real).unwrap();
        let before = "model_reasoning_effort = \"high\"\n";
        std::fs::write(real.join("config.toml"), before).unwrap();

        let home = crate::scratch::root("agent-trust-home");
        std::fs::create_dir_all(&home).unwrap();

        let cwd = std::path::Path::new("/worktrees/some-task");
        write_trust(&codex_trust(), Some(&real), &home, cwd);

        let written = read_lane_config(&home);
        assert_eq!(
            written["model_reasoning_effort"].as_str(),
            Some("high"),
            "the person's own config was dropped"
        );
        assert_eq!(
            written["projects"]["/worktrees/some-task"]["trust_level"].as_str(),
            Some("trusted"),
            "the worktree was not trusted: {written}"
        );
        assert!(
            !std::fs::symlink_metadata(home.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the lane's config is a link, so the entry went into the real home"
        );
        assert_eq!(
            std::fs::read_to_string(real.join("config.toml")).unwrap(),
            before,
            "the real config was written to"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A directory the person's config already speaks about is *set*, not
    /// appended to. `codex exec` writes a `projects` table for every directory
    /// it runs in, so this is the ordinary case for a project root, not a rare
    /// one — and a second table of the same name is a duplicate key that costs
    /// the lane the whole config rather than only the answer.
    #[test]
    fn a_directory_already_in_the_config_is_not_written_twice() {
        let real = crate::scratch::root("agent-trust-dup-real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("config.toml"),
            "[projects.\"/worktrees/some-task\"]\ntrust_level = \"untrusted\"\n",
        )
        .unwrap();

        let home = crate::scratch::root("agent-trust-dup-home");
        std::fs::create_dir_all(&home).unwrap();

        let cwd = std::path::Path::new("/worktrees/some-task");
        write_trust(&codex_trust(), Some(&real), &home, cwd);

        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            text.parse::<toml::Table>().is_ok(),
            "the lane's config does not parse, so codex would refuse it outright: {text}"
        );
        let written = read_lane_config(&home);
        assert_eq!(
            written["projects"]["/worktrees/some-task"]["trust_level"].as_str(),
            Some("trusted"),
            "an existing answer for this directory was left standing: {text}"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A path is a key, not a line of text: dots do not split it into tables,
    /// and a backslash is not an escape. Windows brings both at once.
    #[test]
    fn a_path_with_dots_and_backslashes_stays_one_key() {
        let home = crate::scratch::root("agent-trust-path-home");
        std::fs::create_dir_all(&home).unwrap();

        let cwd = std::path::Path::new(r"C:\Users\marvin\wt\v0.2.0");
        write_trust(&codex_trust(), None, &home, cwd);

        let written = read_lane_config(&home);
        assert_eq!(
            written["projects"][r"C:\Users\marvin\wt\v0.2.0"]["trust_level"].as_str(),
            Some("trusted"),
            "the path was split or unescaped: {written}"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A config spoolway cannot read is handed on as it stands. The lane then
    /// gets the agent's own complaint about it, rather than running against a
    /// config spoolway invented with no provider in it.
    #[test]
    fn a_config_that_does_not_parse_is_passed_through_untouched() {
        let real = crate::scratch::root("agent-trust-bad-real");
        std::fs::create_dir_all(&real).unwrap();
        let broken = "this is not = = toml\n";
        std::fs::write(real.join("config.toml"), broken).unwrap();

        let home = crate::scratch::root("agent-trust-bad-home");
        std::fs::create_dir_all(&home).unwrap();

        write_trust(
            &codex_trust(),
            Some(&real),
            &home,
            std::path::Path::new("/worktrees/some-task"),
        );

        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            broken,
            "a config spoolway could not read was rewritten anyway"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// codex asks about a newer release of itself on startup and holds the
    /// screen until somebody answers, so a lane is launched with the check
    /// off. Held here because the lane that gets this wrong does not fail —
    /// it stalls, and the launch is only reported as a timeout.
    #[test]
    fn a_codex_lane_is_launched_with_the_update_check_off() {
        let codex = adapter("codex").unwrap();
        let args = codex.args.join(" ");
        assert!(
            args.contains("check_for_update_on_startup=false"),
            "a codex lane would stop on the update dialog: {args}"
        );
    }

    fn codex_trust() -> Trust {
        Trust {
            file: "config.toml",
            key: &["projects", "{dir}", "trust_level"],
            value: "trusted",
        }
    }

    fn read_lane_config(home: &std::path::Path) -> toml::Value {
        std::fs::read_to_string(home.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap()
    }

    /// A seed file is linked, not copied, so a credential rotated in the real
    /// home — written to a temp name and renamed into place, the ordinary
    /// shape — is still valid through the per-session home (review finding
    /// 63). On Unix the link is a symlink; the copy fallback is only for a
    /// platform that will not make one.
    #[cfg(unix)]
    #[test]
    fn link_seed_links_so_a_rotation_stays_valid() {
        let real = crate::scratch::root("agent-seed-real");
        let _ = std::fs::remove_dir_all(&real);
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("auth.json"), "first").unwrap();

        let session = crate::scratch::root("agent-seed-session");
        let _ = std::fs::remove_dir_all(&session);
        std::fs::create_dir_all(&session).unwrap();

        link_seed(&real.join("auth.json"), &session.join("auth.json"));

        assert!(
            std::fs::symlink_metadata(session.join("auth.json"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the seed was copied, not linked"
        );

        std::fs::write(real.join("auth.json.new"), "rotated").unwrap();
        std::fs::rename(real.join("auth.json.new"), real.join("auth.json")).unwrap();
        assert_eq!(
            std::fs::read_to_string(session.join("auth.json")).unwrap(),
            "rotated",
            "a link would follow the rotation; a stale copy would not"
        );

        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_dir_all(&session);
    }
}
