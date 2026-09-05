//! `spoolway agent`: the adapter roster, and `verify`'s clause-by-clause
//! audit of one kind against a live binary.

use super::*;

/// Every kind spoolway can start, in table order.
pub(super) fn launchable() -> Vec<&'static str> {
    crate::agent::ADAPTERS
        .iter()
        .filter(|adapter| adapter.launches())
        .map(|adapter| adapter.kind)
        .collect()
}

/// Every kind spoolway knows, and what each one can do here.
///
/// The roster, deliberately over the whole adapter table rather than over the
/// profiles this project configures — `doctor` covers those, and the kind
/// somebody runs this to ask about is precisely the one no profile names yet.
/// Reads nothing but the table and `PATH`, so it answers the same in a project
/// and outside one.
pub fn agent_list(json: bool) -> Result<()> {
    let rows: Vec<(&crate::agent::Adapter, Option<String>)> = crate::agent::ADAPTERS
        .iter()
        .map(|adapter| (adapter, which(adapter.program())))
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&agent_list_json(&rows))?);
        return Ok(());
    }

    let width = rows
        .iter()
        .map(|(adapter, _)| adapter.kind.len())
        .max()
        .unwrap_or(4)
        .max("KIND".len());

    println!(
        "{:<width$}  {:<10}  {:<18}  BINARY",
        "KIND", "LAUNCH", "ACCOUNTING"
    );
    for (adapter, binary) in &rows {
        let launch = match adapter.launches() {
            true => "ok",
            false => "no args",
        };
        // Two different sentences, and the difference is the point: a kind
        // that cannot be launched is unmetered the way an empty row is
        // unmetered, and one that can is unmetered as a state it ships in.
        let accounting = match (adapter.meters(), adapter.launches()) {
            (true, _) => "ok",
            (false, true) => "none — unmetered",
            (false, false) => "none",
        };
        println!(
            "{:<width$}  {launch:<10}  {accounting:<18}  {}",
            adapter.kind,
            binary.as_deref().unwrap_or("not on PATH")
        );
    }

    let launchable: Vec<&str> = crate::agent::ADAPTERS
        .iter()
        .filter(|a| a.launches())
        .map(|a| a.kind)
        .collect();
    let metered = launchable
        .iter()
        .filter(|kind| crate::agent::adapter(kind).is_some_and(|a| a.meters()))
        .count();

    println!();
    println!(
        "{} of {} kinds can be launched. {metered} of those are metered.",
        launchable.len(),
        crate::agent::ADAPTERS.len()
    );
    println!("An unmetered kind runs: no ledger, no session reuse, coarse silence.");
    println!("`spoolway agent verify <kind>` says which clause is missing.");
    Ok(())
}

/// `agent_list`'s `--json` rows, pulled out as a pure function rather than
/// built inline — `agent_list` itself only ever prints, and a test asserting
/// on that output would need to capture stdout, which nothing in this
/// codebase does. A function that returns the rows instead is what let the
/// test below check the actual shape.
fn agent_list_json(rows: &[(&crate::agent::Adapter, Option<String>)]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|(adapter, binary)| {
            serde_json::json!({
                "kind": adapter.kind,
                "launches": adapter.launches(),
                "meters": adapter.meters(),
                "binary": binary,
            })
        })
        .collect()
}

/// One clause of the contract, and what checking it found.
///
/// Three weights rather than two, because the whole shape of this command is
/// that most of what can be wrong with a kind is not a fault. Only
/// [`Clause::Fail`] moves the exit code, and only the launch half ever
/// produces one.
enum Clause {
    Ok(String),
    /// A stated cost, with the lines that name it. Reported at length and
    /// exits 0 — an unmetered kind is a legal state, not a broken one.
    Warn(String, Vec<String>),
    Fail(String),
}

impl Clause {
    fn print(&self, label: &str) {
        match self {
            Clause::Ok(note) if note.is_empty() => println!("ok    {label}"),
            Clause::Ok(note) => println!("ok    {label} — {note}"),
            Clause::Warn(note, detail) => {
                println!("warn  {label} — {note}");
                for line in detail {
                    println!("        · {line}");
                }
            }
            Clause::Fail(why) => println!("FAIL  {label} — {why}"),
        }
    }

    /// The same reading `print` writes to a terminal, structured instead —
    /// `--json`'s own view of one clause.
    fn to_json(&self, label: &str) -> serde_json::Value {
        match self {
            Clause::Ok(note) => serde_json::json!({"label": label, "status": "ok", "note": note}),
            Clause::Warn(note, detail) => {
                serde_json::json!({"label": label, "status": "warn", "note": note, "detail": detail})
            }
            Clause::Fail(why) => serde_json::json!({"label": label, "status": "fail", "why": why}),
        }
    }
}

/// Check one kind against its row, clause by clause.
///
/// Every clause is reported separately because they fail separately and cost
/// different things: a missing `args` row means the kind cannot start at all,
/// while a missing accounting row means it starts and runs and simply is not
/// counted. The exit code follows the launch half alone.
///
/// `project` is what the command was run inside, when it was run inside one.
/// It is consulted for exactly one thing — a model for `--live`, since
/// spoolway names none of its own — so the whole command works outside a
/// project, which is where somebody asking "could I run codex?" often is.
pub fn agent_verify(
    args: &AgentVerifyArgs,
    project: Option<(&Repo, &Pipelines)>,
    json: bool,
) -> Result<()> {
    // `--live` spawns real turns and reports on them clause by clause as it
    // goes, in `agent_verify_live` below — folding that into one JSON payload
    // is its own piece of work this task's scope does not reach, so the two
    // flags are refused together rather than one of them silently winning.
    if json && args.live {
        bail!("`--json` does not cover `--live` yet — run them separately");
    }

    let Some(adapter) = crate::agent::adapter(&args.kind) else {
        bail!(
            "spoolway knows no agent kind `{}` — known kinds: {}",
            args.kind,
            crate::agent::ADAPTERS
                .iter()
                .map(|a| a.kind)
                .collect::<Vec<_>>()
                .join(", ")
        );
    };

    let mut failed = 0;
    let mut clauses: Vec<serde_json::Value> = Vec::new();
    let mut report = |label: &str, clause: Clause| {
        if matches!(clause, Clause::Fail(_)) {
            failed += 1;
        }
        match json {
            true => clauses.push(clause.to_json(label)),
            false => clause.print(label),
        }
    };

    // ---------------------------------------------------------- the launch half
    let binary = which(adapter.program());
    report(
        "binary on PATH",
        match &binary {
            Some(path) => Clause::Ok(path.clone()),
            None => Clause::Fail(format!(
                "`{}` is not on PATH, so nothing here could start it",
                adapter.program()
            )),
        },
    );

    let mut carries = Vec::new();
    if !adapter.args.is_empty() {
        carries.push("args");
    }
    carries.push(match adapter.permissions.is_some() {
        true => "permissions",
        false => "permissions: none",
    });
    carries.push(match adapter.effort.is_some() {
        true => "effort",
        false => "effort: none",
    });
    carries.push(match adapter.headless.is_some() {
        true => "headless",
        false => "headless: none",
    });
    report(
        "launch row",
        match adapter.launches() {
            true => Clause::Ok(carries.join(", ")),
            false => Clause::Fail(
                "no `args` template in `agent::ADAPTERS`, so there is nothing to launch it \
                 with — a profile naming this kind is refused rather than run with no flags"
                    .into(),
            ),
        },
    );

    // Rendered through the very function a lane start renders through, against
    // the placeholder set the dispatcher fills, so what passes here is what
    // will pass there.
    let profile = crate::config::AgentProfile::for_kind(&args.kind);
    let session = crate::usage::new_session_id();
    let rendered = match adapter.launches() {
        true => profile.render_args(&sample_placeholders(&session)),
        false => Err(anyhow::anyhow!("the launch row is empty")),
    };
    report(
        "args render",
        match (&rendered, adapter.launches()) {
            (Ok(args), _) => Clause::Ok(format!("{} args, no unknown placeholders", args.len())),
            (Err(_), false) => Clause::Ok("nothing to render".into()),
            (Err(err), true) => Clause::Fail(format!("{err:#}")),
        },
    );

    // How this lane's session is told apart from every other session of this
    // kind on the machine. Only reported for a kind that pins by home, because
    // for every other kind the answer is the `{session_id}` the args render
    // already checked.
    if let Some(home) = adapter.home.as_ref() {
        report(
            "session pinned by",
            match crate::agent::session_home(&args.kind, &session) {
                Some(dir) => Clause::Ok(format!("${} → {}", home.env, dir.display())),
                None => Clause::Fail(
                    "no state directory, so no per-session home can be made — this kind \
                     would continue whichever session ran last on this machine"
                        .into(),
                ),
            },
        );
    }

    report(
        "resume rewrite",
        resume_clause(adapter, &rendered, &session),
    );

    // ------------------------------------------------------ the accounting half
    // Nothing below can fail the command. Every one of these is a reading a
    // lane already degrades gracefully without — see `agent::Accounting`.
    let unmetered = vec![
        "no ledger lines, so `spoolway spend` will not see its lanes".to_string(),
        "no session reuse — every step starts a fresh session".into(),
        "silence is detected from the pane, not the transcript, which is weak on the \
         headless backend"
            .into(),
    ];
    match &adapter.accounting {
        Some(accounting) => report(
            "accounting row",
            Clause::Ok(format!(
                "{}, {}, {}{}",
                match adapter.home.as_ref() {
                    Some(home) => format!("${}", home.env),
                    None => format!("~/{}", accounting.sessions_dir),
                },
                accounting.store.label(),
                accounting.format.label(),
                match accounting.session_env {
                    Some(var) => format!(", ${var}"),
                    None => String::new(),
                }
            )),
        ),
        None => report(
            "accounting row",
            Clause::Warn("none, so this kind is unmetered:".into(), unmetered),
        ),
    }

    report("transcript directory", transcript_dir_clause(adapter));

    // Quota is drawn in its own shape rather than through `report` above —
    // the mockup's own `claude   quota  ...` row, not the generic `ok/warn
    // quota — ...` line every other clause here uses — so a person reading
    // several kinds' output in a row sees which kind each quota line is
    // about without having to scroll back to the command that produced it.
    let quota = quota_clause(adapter);
    if matches!(quota, Clause::Fail(_)) {
        failed += 1;
    }
    match json {
        true => clauses.push(quota.to_json("quota")),
        false => print_quota_row(&args.kind, &quota),
    }

    if json {
        let payload = serde_json::json!({"kind": args.kind, "failed": failed, "clauses": clauses});
        println!("{}", serde_json::to_string_pretty(&payload)?);
        if failed > 0 {
            bail!(
                "{failed} clause(s) of the launch half failed — `{}` cannot be run here",
                args.kind
            );
        }
        return Ok(());
    }

    // ------------------------------------------------------------------- closing
    println!();
    // The launch half first, and it is a gate on the rest: a turn cannot be
    // run through a row that does not launch, and trying would report the
    // spawn's own failure in place of the clause that really went wrong.
    if failed > 0 {
        if args.live {
            println!("note  --live did not run: the launch half failed above.");
            println!();
        }
        bail!(
            "{failed} clause(s) of the launch half failed — `{}` cannot be run here",
            args.kind
        );
    }

    if args.live {
        return agent_verify_live(adapter, args, project, &rendered);
    }
    if adapter.meters() {
        println!("note  the transcript readings need a real turn — re-run with --live");
        println!();
    }

    match adapter.meters() {
        true => println!(
            "{} meets the contract on every clause checked here.",
            args.kind
        ),
        false => println!(
            "{} can be launched. It cannot be accounted for. Exit 0.",
            args.kind
        ),
    }
    Ok(())
}

/// The placeholders a lane start fills, with values that are obviously not a
/// lane's.
///
/// Every key the dispatcher passes has to be here or the render fails on an
/// unknown placeholder — which is the whole check. Paths that do not exist are
/// deliberate: this renders args, it does not run them.
fn sample_placeholders(session: &str) -> std::collections::BTreeMap<&'static str, String> {
    std::collections::BTreeMap::from([
        ("model", "<model>".to_string()),
        ("session_id", session.to_string()),
        ("prompt_file", "/<worktree>/prompt.md".to_string()),
        ("task_file", "/<state>/queue/<task>.md".to_string()),
        ("worktree", "/<worktree>".to_string()),
        ("repo", "/<repo>".to_string()),
        ("state_dir", "/<repo>/.spoolway".to_string()),
        // The task file's new home — see `crate::repo::Repo::home` — which a
        // lane reads from outside its worktree the same way it reads
        // `state_dir` for its prompts.
        ("project_home", "/<home>/.spoolway/<project>".to_string()),
    ])
}

/// Whether a second turn of this kind really re-addresses the session the
/// first one opened.
///
/// The one clause whose failure is silent in production: a rewrite that finds
/// no session flag to change hands the binary a fresh session while claiming
/// to continue one, and the lane answers a question with none of the context
/// the question came from. So it is checked on the args this kind actually
/// renders, and a miss is a launch-half failure rather than a note.
fn resume_clause(
    adapter: &crate::agent::Adapter,
    rendered: &Result<Vec<String>>,
    session: &str,
) -> Clause {
    let Some(headless) = adapter.headless.as_ref() else {
        return Clause::Warn(
            "no headless row, so this kind runs only under a multiplexer:".into(),
            vec![
                "`dispatch.backend = \"headless\"` refuses to start it rather than guessing \
                 a flag at a real binary"
                    .into(),
                "a step with `session: true` opens fresh every time".into(),
            ],
        );
    };
    let Ok(rendered) = rendered else {
        return Clause::Ok("nothing to rewrite".into());
    };

    // A `Tail` kind never names its session in the argv — there is no id in
    // there to survive anything. What has to hold instead is that a later turn
    // is not the same argv as the first, since for this kind an unchanged argv
    // is precisely what opens a fresh session.
    if let crate::agent::Resume::Tail(tokens) = &headless.resume {
        let first = adapter.headless_args(rendered, false).unwrap_or_default();
        let later = adapter.headless_args(rendered, true).unwrap_or_default();
        if first == later {
            return Clause::Fail(format!(
                "`{}` leaves the argv unchanged, so a second turn would open a fresh \
                 session while claiming to continue one",
                tokens.join(" ")
            ));
        }
        return match adapter.home.as_ref() {
            Some(home) => Clause::Ok(format!(
                "appends `{}` after `{}`, pinned to one session by ${}",
                tokens.join(" "),
                headless.print.tokens().join(" "),
                home.env
            )),
            // The tail alone means "the most recent session on this machine",
            // which is not this lane's — the home is what narrows it.
            None => Clause::Fail(format!(
                "`{}` resumes the most recent session on this machine, and this row \
                 sets no home to narrow that to the lane's own",
                tokens.join(" ")
            )),
        };
    }

    let resumed = headless.resume.apply(rendered);
    if !resumed.iter().any(|arg| arg == session) {
        return Clause::Fail(
            "the session id does not survive the rewrite, so a second turn would open a \
             fresh session while claiming to continue one"
                .into(),
        );
    }
    Clause::Ok(match &headless.resume {
        crate::agent::Resume::Tail(_) => unreachable!("handled above"),
        crate::agent::Resume::SameArgs => {
            "the session flag creates-or-continues, so a later turn is the same argv".into()
        }
        crate::agent::Resume::Swap { from, to } => {
            match resumed.iter().any(|arg| arg == to) && !resumed.iter().any(|arg| arg == from) {
                true => format!("rewrites `{from}` to `{to}`, id in place"),
                false => format!("`{from}` → `{to}` — but the rendered args carry neither"),
            }
        }
    })
}

/// Whether this kind's transcripts have anywhere to land on this machine.
fn transcript_dir_clause(adapter: &crate::agent::Adapter) -> Clause {
    let Some(accounting) = adapter.accounting.as_ref() else {
        return Clause::Ok("none to check — this kind writes nothing spoolway reads".into());
    };
    let Some(home) = crate::platform::home_dir() else {
        return Clause::Warn(
            "no home directory, so the transcript root cannot be resolved:".into(),
            vec!["every reading off a transcript will come back empty".into()],
        );
    };
    // A kind that pins by home writes under spoolway's state directory, not
    // its own — that relocation is the whole mechanism, so reporting the
    // agent's default home here would name a directory nothing will ever
    // write to.
    let root = match adapter.home.as_ref().map(|home| home.env) {
        Some(var) => {
            let Some(state) = crate::usage::state_root() else {
                return Clause::Warn(
                    "no state directory, so the per-session homes cannot be rooted:".into(),
                    vec!["every reading off a transcript will come back empty".into()],
                );
            };
            let root = state.join(accounting.sessions_dir);
            return match root.is_dir() {
                true => Clause::Ok(format!("{} (${var} per session)", root.display())),
                false => Clause::Warn(
                    format!("{} does not exist yet:", root.display()),
                    vec![format!(
                        "no lane of this kind has run on this machine — spoolway makes each \
                         session's ${var} at lane start"
                    )],
                ),
            };
        }
        None => home.join(accounting.sessions_dir),
    };
    match root.is_dir() {
        true => Clause::Ok(root.display().to_string()),
        false => Clause::Warn(
            format!("{} does not exist yet:", root.display()),
            vec![
                "this kind has written no session on this machine, so there is nothing to \
                 read back until it has run once"
                    .into(),
            ],
        ),
    }
}

/// Where this kind's own cached usage percentage comes from, and what it
/// currently reads — see [`crate::agent::Adapter::quota`] and
/// [`crate::quota`]. Never a [`Clause::Fail`]: every one of the four ways
/// this can come up short — no probe, an unreadable or unparseable source, a
/// reading judged stale, or (codex only) a reading with nothing behind it —
/// is exactly the fail-open case `dispatch.rs`'s own gates already degrade
/// past, so it is reported here and never blocks the command.
///
/// The two established rows read a different shape off a different place, so
/// this branches on [`crate::agent::Accounting::format`] rather than
/// assuming `claude`'s file-and-clock-time display is universal — codex has
/// no single cache file to name, and its own mockup reads the reset as a
/// countdown rather than a clock, since the account it describes may not be
/// signed in from this machine's own timezone at all.
fn quota_clause(adapter: &crate::agent::Adapter) -> Clause {
    let Some(rel) = adapter.quota else {
        // The embedded `\n` is the mockup's own line break, not a wrap this
        // reader chose — `render_quota_row` indents whatever follows it to
        // the note's own column, the same as the two-window `Ok` case does.
        return Clause::Warn(
            "no probe established — lanes of this kind are never parked\nfor quota, and its \
             usage limit is not detected either"
                .into(),
            Vec::new(),
        );
    };
    let is_codex = matches!(
        adapter.accounting.as_ref().map(|a| a.format),
        Some(crate::usage::Format::Codex)
    );
    // The mockup's own two-line source description for codex, split the same
    // way a two-window `Ok` case splits its lines — `render_quota_row`
    // indents every one of them to the note's own column.
    let source = match is_codex {
        true => "newest rollout under the lane's CODEX_HOME,\nlast token_count event's rate_limits"
            .to_string(),
        false => format!("~/{rel} cachedUsageUtilization"),
    };
    match crate::quota::read(adapter.kind) {
        Err(crate::quota::Miss::NoProbe) => unreachable!("adapter.quota was just Some"),
        Err(crate::quota::Miss::Unreadable(why)) => {
            Clause::Warn(format!("{source} could not be read: {why}"), Vec::new())
        }
        Err(crate::quota::Miss::Unparseable(why)) => {
            Clause::Warn(format!("{source} did not parse: {why}"), Vec::new())
        }
        // codex's own negative case — a rollout that parsed cleanly and
        // carries nothing to act on, never a malformed file.
        Err(crate::quota::Miss::NoReading(why)) => {
            Clause::Warn(format!("{source}\n{why}"), Vec::new())
        }
        Ok(reading) if reading.stale(chrono::Utc::now()) => Clause::Warn(
            format!("{source} is stale — fetched too long ago to trust; treated as absent"),
            Vec::new(),
        ),
        Ok(reading) => {
            let now = chrono::Utc::now();
            let window = |w: &crate::quota::WindowReading| match is_codex {
                // codex's own two windows are `primary`/`secondary` on the
                // wire, not `five_hour`/`seven_day` — those labels are
                // claude's own key names, kept for the reading's internal
                // bookkeeping (`Window::SevenDay` still decides the park's
                // own date shape in `dispatch.rs`) but not what a person
                // reading `agent verify`'s codex row should see.
                //
                // And the reset itself reads as a countdown rather than a
                // clock: the account behind this reading is not necessarily
                // this machine's own local time, where claude's cache always
                // is.
                true => {
                    let label = match w.window {
                        crate::quota::Window::FiveHour => "primary",
                        crate::quota::Window::SevenDay => "secondary",
                    };
                    let remaining = (w.resets_at.timestamp() - now.timestamp()).max(0) as u64;
                    format!(
                        "{label} {}% resets in {}",
                        w.utilization,
                        crate::config::human_duration::format(std::time::Duration::from_secs(
                            remaining
                        )),
                    )
                }
                // A reset today reads as a bare clock the same way the board
                // and the dispatcher's own reports do; a reset on another day
                // is named `MM-DD HH:MM` rather than either of
                // `format_instant`'s own shapes — this line already carries
                // "resets", so a year nobody asked about would only crowd
                // it, and this display names no park for `parked_window` to
                // pin a longer one against.
                false => {
                    let (target, same_day) =
                        crate::task::local_instant(w.resets_at.timestamp(), now.timestamp());
                    let resets = match same_day {
                        true => target.format("%H:%M").to_string(),
                        false => target.format("%m-%d %H:%M").to_string(),
                    };
                    format!("{} {}% resets {resets}", w.window.key(), w.utilization)
                }
            };
            // No indent baked in here — this note is also `--json`'s own
            // payload, which has no notion of a printed column to align to.
            // `print_quota_row` is what indents a continuation line, to
            // whatever width its own kind-name column comes out to.
            Clause::Ok(format!(
                "{source}\n{} · {}",
                window(&reading.five_hour),
                window(&reading.seven_day),
            ))
        }
    }
}

/// Prints the quota clause in its own shape — the mockup's own
/// `claude   quota  ...` row — rather than through [`Clause::print`]'s
/// generic `ok`/`warn`/`FAIL` line every other clause in this command uses.
/// Quota is the only clause the mockup pins to an exact rendering.
///
/// Prints [`render_quota_row`] — split out as a pure function, the same way
/// `agent_list_json` is, so a test can check the actual text without
/// capturing stdout, which nothing else in this codebase does.
fn print_quota_row(kind: &str, clause: &Clause) {
    println!("{}", render_quota_row(kind, clause));
}

/// The mockup's own `claude   quota  ...` row: a kind name left-aligned and
/// padded on the right to the width of the longest kind in the whole
/// adapter table, so every kind's own row lines up the same way whichever
/// one is checked, then `quota`, then the note — split on its own embedded
/// `\n` (see [`quota_clause`]'s `Ok` case) with each continuation line
/// indented to the note's own column rather than the kind's.
fn render_quota_row(kind: &str, clause: &Clause) -> String {
    let width = crate::agent::ADAPTERS
        .iter()
        .map(|a| a.kind.len())
        .max()
        .unwrap_or(0);
    let prefix = format!("{kind:<width$}   quota  ");
    let indent = " ".repeat(prefix.chars().count());
    // quota_clause's own doc: never a Fail. Matched exhaustively rather
    // than assumed, in case that ever changes.
    let (note, detail): (&str, &[String]) = match clause {
        Clause::Ok(note) => (note.as_str(), &[]),
        Clause::Warn(note, detail) => (note.as_str(), detail.as_slice()),
        Clause::Fail(why) => (why.as_str(), &[]),
    };
    let mut lines = note.split('\n');
    let mut out = format!("{prefix}{}", lines.next().unwrap_or_default());
    for line in lines {
        out.push('\n');
        out.push_str(&indent);
        out.push_str(line);
    }
    for line in detail {
        out.push('\n');
        out.push_str(&indent);
        out.push_str("· ");
        out.push_str(line);
    }
    out
}

/// Whether a turn actually wrote into the per-session home spoolway made it,
/// and which home that was. `None` for a kind that pins by id, which has none.
///
/// A kind with a `home` at all relocates a whole tree, so "written" means
/// anything the agent put there — which is anything that is not one of the
/// credentials spoolway linked in before it started. Counting those would
/// answer "did spoolway seed this home", which is a question about spoolway
/// rather than about the turn.
fn home_written(kind: &str, session: &str) -> Option<(std::path::PathBuf, bool)> {
    let home = crate::agent::session_home(kind, session)?;
    let row = crate::agent::adapter(kind)?.home.as_ref()?;
    let wrote = std::fs::read_dir(&home).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            !row.seed
                .contains(&entry.file_name().to_string_lossy().as_ref())
        })
    });
    Some((home, wrote))
}

/// What the live check's prompt asks the model to say back.
///
/// Deliberately not a word a model would produce on its own: seeing it in the
/// reply means the prompt was read, and that is the only evidence there is.
const PROMPT_TOKEN: &str = "SPOOLWAY-OK";

/// The prompt a live check spends its turn on.
///
/// Deliberately close to the smallest thing that still produces an assistant
/// turn: what is under test is the launch and the transcript, not the model,
/// and every token past a short line is spent on nothing.
///
/// It asks for one line rather than for one *word*, and that is not a detail:
/// a prompt that dictates the whole reply leaves no room for the prompt's own
/// instruction, and the prompt clause above then reads "did not comply" on a
/// model that complied with the last thing it was told. The two have to be able
/// to hold at once.
const LIVE_PROMPT: &str = "Say hello, in one short line.";

/// The prompt the resumed turn spends. As small as the first, and different
/// from it, so what lands in the transcript is unmistakably a second turn
/// rather than a replay of the first.
const RESUME_PROMPT: &str = "Say goodbye, in one short line.";

/// How long one live turn is given before it is killed.
///
/// A real turn of a small model is seconds; a turn still going after several
/// minutes has hung, and `verify --live` is often run from CI where a hang
/// is a stuck job with nobody to Ctrl-C it (review finding 62). Generous
/// enough that a slow-but-working turn still finishes on its own.
const LIVE_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// One real turn of a live check, read back the way a lane's own turn is.
struct LiveTurn {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    /// Whether the turn was killed for running past [`LIVE_TURN_TIMEOUT`]
    /// rather than exiting on its own.
    timed_out: bool,
}

/// Run one turn of `program`, capturing both streams the way
/// `std::process::Command::output` does — but bounded by
/// [`LIVE_TURN_TIMEOUT`], so a hung agent is killed rather than waited on
/// forever (review finding 62).
///
/// The two streams are drained by threads of their own so a turn that fills
/// a pipe buffer cannot deadlock the wait, the same shape
/// `Command::output` uses internally.
fn run_live_turn(
    program: &str,
    argv: &[String],
    prompt: &str,
    dir: &Path,
    env: Vec<(String, String)>,
) -> Result<LiveTurn> {
    use std::io::Read;

    let mut child = std::process::Command::new(program)
        .args(argv)
        .arg(prompt)
        .current_dir(dir)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running `{program}`"))?;

    let mut out = child.stdout.take().expect("stdout was piped");
    let mut err = child.stderr.take().expect("stderr was piped");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + LIVE_TURN_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting on `{program}`"))?
        {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            // `kill` reaches the child spoolway spawned, not a process group —
            // a stray grandchild it left holding a pipe open can still delay
            // the `join` below. Bounded enough for a diagnostic command: the
            // wait that used to be unbounded was on the child's own idle turn.
            let _ = child.kill();
            timed_out = true;
            break child
                .wait()
                .with_context(|| format!("waiting on `{program}` after the deadline"))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();

    Ok(LiveTurn {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

/// Run one real turn, read the transcript back, then resume it once.
///
/// The half of the contract no file on disk can settle. A kind can carry a
/// perfect accounting row and write a transcript in a shape the parser no
/// longer recognises — every reading comes back empty, every lane of that kind
/// silently goes unaccounted and resumes nothing, and nothing anywhere goes
/// red. One turn is what says otherwise — and one more, resumed, is what says
/// the resume spelling still continues a session on the binary installed
/// today rather than on the version the row was settled against.
///
/// Unlike everything above it, a reading that fails here **does** fail the
/// command: an absent accounting row is a legal state, but a declared one that
/// does not read back is a wrong row.
/// A scratch tree — and the per-session agent home the check makes it — that
/// go when it falls out of scope.
///
/// A guard rather than a `remove_dir_all` at the end of the check, because the
/// check has a dozen ways out: every `?` on a spawn that would not start, the
/// `bail!` on a reading that did not match, and the ordinary pass. An end-of-
/// function call would clean up after exactly the run that needed it least.
///
/// `home` is the state directory `prepare_session_home` makes for a kind that
/// mints its own session id — `None` for every other kind. It was never
/// reclaimed before, so CI running `verify --live` per push left one behind
/// on every run (review finding 62).
struct ScratchTree {
    dir: std::path::PathBuf,
    home: Option<std::path::PathBuf>,
}

impl Drop for ScratchTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        if let Some(home) = &self.home {
            let _ = std::fs::remove_dir_all(home);
        }
    }
}

fn agent_verify_live(
    adapter: &crate::agent::Adapter,
    args: &AgentVerifyArgs,
    project: Option<(&Repo, &Pipelines)>,
    rendered: &Result<Vec<String>>,
) -> Result<()> {
    // A skip is not a failure. `--live` asked for the one thing this kind
    // cannot do, and the launch half already passed — so it says which thing,
    // and closes on the same line a run without `--live` would have.
    let skip = |why: &str| -> Result<()> {
        println!("note  --live did not run: {why}. Nothing was started and nothing spent.");
        println!();
        println!(
            "{} can be launched. It cannot be accounted for. Exit 0.",
            args.kind
        );
        Ok(())
    };

    if adapter.headless.is_none() {
        return skip("this kind has no headless row, so there is no way to hand it one prompt");
    }
    if rendered.is_err() {
        return skip("its args do not render");
    }
    // An unmetered kind is *not* skipped. There is no transcript to read back,
    // but the turn itself is worth running: it is the only thing that says the
    // args, the prompt and the per-session home reach a real binary and come
    // back with an answer — which for a kind with no accounting is the whole of
    // what can be checked, and is exactly what nothing else checks.

    let model = args
        .model
        .clone()
        .or_else(|| live_model(&args.kind, project))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--live needs a model and nothing here names one for `{}`. spoolway names none \
                 for you: pass `--model <name>`, or run this in a project whose pipelines have \
                 a step on this kind",
                args.kind
            )
        })?;

    // A scratch tree, not the project: a live check is spoolway asking a
    // binary a question about itself, and it has no business opening a session
    // in a checkout somebody is working in.
    let session = crate::usage::new_session_id();
    let dir = std::env::temp_dir().join(format!("spoolway-verify-{session}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Taken back on the way out, whichever way that is. The session id in the
    // name never repeats, so nothing would ever have reused or reclaimed one,
    // and a check that is run often enough to be useful is run often enough to
    // matter: one machine held 531 of these trees, the oldest two weeks old.
    // The per-session agent home, if this kind gets one, is added below once
    // its path is known.
    let mut scratch = ScratchTree {
        dir: dir.clone(),
        home: None,
    };
    // A repo, because a lane's worktree is one and at least one kind checks:
    // codex refuses to start outside a git repo with "Not inside a trusted
    // directory". A scratch tree that is not a repo would fail this kind on the
    // launch it was not asked about, and report it as an accounting failure.
    let _ = crate::repo::run(&dir, "git", &["init", "-q"]);
    // The prompt asks for a token back, because the prompt is the one thing a
    // lane cannot run without that no file on disk can confirm arrived: every
    // kind takes it a different way — a flag, a config override, a whole config
    // document in the environment — and each of those can be accepted by the
    // binary and then quietly ignored. A word in the reply is what says
    // otherwise. See [`PROMPT_TOKEN`].
    let prompt_text = format!(
        "You are answering one check of spoolway's agent contract.\nBegin your reply with \
         the exact token {PROMPT_TOKEN}, then answer in one short line.\n"
    );
    let prompt_file = dir.join("prompt.md");
    std::fs::write(&prompt_file, &prompt_text)?;
    let task_file = dir.join("task.md");
    std::fs::write(&task_file, "# verify\n\nNothing to do.\n")?;

    let profile = crate::config::AgentProfile::for_kind(&args.kind);
    let values = std::collections::BTreeMap::from([
        ("model", model.clone()),
        ("session_id", session.clone()),
        ("prompt_file", prompt_file.display().to_string()),
        ("task_file", task_file.display().to_string()),
        ("worktree", dir.display().to_string()),
        ("repo", dir.display().to_string()),
        ("state_dir", dir.display().to_string()),
        ("project_home", dir.display().to_string()),
    ]);
    let rendered_args = profile.render_args(&values)?;
    // Composed by the adapter, exactly as a headless lane's first turn is —
    // args, then the print form, then the prompt — so this exercises the launch
    // path rather than a second spelling of it.
    let argv = adapter
        .headless_args(&rendered_args, false)
        .context("this kind has no headless row")?;

    // The same home a lane of this kind would be given, made the same way. For
    // a kind that pins by id this is `None` and nothing below changes; for one
    // that pins by home, the guard now owns it and takes it back on the way
    // out however the check ends (review finding 62).
    scratch.home = crate::agent::prepare_session_home(&args.kind, &session);
    let env: Vec<(String, String)> = adapter.session_env(&session);

    println!(
        "note  one turn of `{model}` on `{}`, in {}",
        args.kind,
        dir.display()
    );
    let started = chrono::Utc::now().timestamp();
    let output = run_live_turn(adapter.program(), &argv, LIVE_PROMPT, &dir, env.clone())?;

    let mut failed = 0;
    let mut report = |label: &str, clause: Clause| {
        if matches!(clause, Clause::Fail(_)) {
            failed += 1;
        }
        clause.print(label);
    };

    report(
        "the turn ran",
        match (output.status.success(), output.timed_out) {
            (true, _) => Clause::Ok(first_line(&output.stdout).to_string()),
            (false, true) => Clause::Fail(format!(
                "`{}` was killed after {}s — it never finished a turn",
                args.kind,
                LIVE_TURN_TIMEOUT.as_secs()
            )),
            (false, false) => Clause::Fail(format!(
                "`{}` exited {} — {}",
                args.kind,
                output.status,
                first_line(&output.stderr)
            )),
        },
    );

    // Whether the prompt reached the model at all — the clause that no file on
    // disk can answer, and the one most worth asking of a kind whose prompt
    // goes in somewhere unusual.
    //
    // A warning rather than a failure when the token is missing, because two
    // very different things produce that: a prompt that never arrived, and a
    // model that read it and did not comply. Small local models really do
    // ignore an instruction like this, and failing the contract over a model's
    // obedience would make the command useless exactly where it is cheapest to
    // run.
    let said = output.stdout.clone();
    report(
        "the prompt reached the turn",
        match said.contains(PROMPT_TOKEN) {
            true => Clause::Ok(format!("`{PROMPT_TOKEN}` came back in the reply")),
            false => Clause::Warn(
                format!("the reply does not carry `{PROMPT_TOKEN}`:"),
                vec![
                    "either the prompt never reached the model, or the model read it and \
                     did not comply — a small local model often will not"
                        .into(),
                    format!("what came back: {}", first_line(&said)),
                ],
            ),
        },
    );

    // A kind that pins by home is only pointed at *its own* session because the
    // home is its own, so the turn has to have written into the one spoolway
    // made. For a metered kind the readings below prove that and more; for an
    // unmetered one this is the only thing left that can be checked — and it is
    // what catches a relocation variable the binary quietly ignored, which
    // would otherwise show up as two lanes continuing each other's session.
    let landed = home_written(&args.kind, &session).map(|(home, wrote)| match wrote {
        true => (Clause::Ok(home.display().to_string()), true),
        false => (
            Clause::Fail(format!(
                "{} is empty, so this turn's session went somewhere else — a later turn \
                 would continue whatever ran last on this machine instead",
                home.display()
            )),
            false,
        ),
    });
    let landed_ok = match landed {
        Some((clause, ok)) => {
            report("the session landed in the home spoolway made", clause);
            ok
        }
        None => true,
    };

    // An unmetered kind stops here, having been asked everything it can answer.
    if adapter.accounting.is_none() {
        println!();
        if !output.status.success() || !landed_ok {
            bail!(
                "`{}` did not come back from a real turn as its row says it would",
                args.kind
            );
        }
        println!(
            "{} launches, runs a turn and pins its session. It cannot be accounted for. Exit 0.",
            args.kind
        );
        return Ok(());
    }

    // Everything below is a reading, and every one of them is reached through
    // exactly the function the dispatcher and the ledger reach it through.
    match crate::usage::session_path(&args.kind, &session) {
        Some(path) => report(
            "session id round-trips",
            Clause::Ok(path.display().to_string()),
        ),
        None => {
            report(
                "session id round-trips",
                Clause::Fail(format!(
                    "nothing under {} carries the session spoolway pinned, so every reading \
                     below is unreachable",
                    match crate::agent::session_home(&args.kind, &session) {
                        Some(home) => home.display().to_string(),
                        None => format!(
                            "~/{}",
                            adapter
                                .accounting
                                .as_ref()
                                .map(|a| a.sessions_dir)
                                .unwrap_or_default()
                        ),
                    }
                )),
            );
            println!();
            bail!(
                "1 reading of the accounting row failed — the row does not match what `{}` wrote",
                args.kind
            );
        }
    }

    report(
        "tokens read back",
        match crate::usage::harvest(&args.kind, &session) {
            Some(harvest) => Clause::Ok(format!(
                "in {} · out {} · cache read {} · cache write {}",
                harvest.tokens.input,
                harvest.tokens.output,
                harvest.tokens.cache_read,
                harvest.tokens.cache_write()
            )),
            None => Clause::Fail(
                "the transcript is there but no assistant turn parsed out of it — this kind's \
                 `Format` no longer matches what it writes"
                    .into(),
            ),
        },
    );

    match crate::usage::last_turn(&args.kind, &session) {
        Some(size) => {
            report(
                "last-turn size reads back",
                Clause::Ok(format!("{size} tokens")),
            );
        }
        None => {
            report(
                "last-turn size reads back",
                Clause::Fail("no last turn parsed — `session_reuse_ctx` would never apply".into()),
            );
        }
    }

    report(
        "mtime moved while the turn ran",
        match crate::usage::last_written(&args.kind, &session) {
            Some(at) if at >= started => Clause::Ok(format!("{}s after it started", at - started)),
            Some(at) => Clause::Fail(format!(
                "the transcript was last written {}s before the turn began, so the reminder \
                 loop would read this lane as quiet while it worked",
                started - at
            )),
            None => Clause::Fail("no mtime — the reminder loop falls back to the pane".into()),
        },
    );

    report(
        "running totals read back",
        match crate::usage::harvest(&args.kind, &session) {
            Some(harvest) => Clause::Ok(format!(
                "{} turn(s), {} output{}",
                harvest.turns,
                harvest.tokens.output,
                match harvest.cost_usd {
                    Some(cost) => format!(", ${cost:.4} self-reported"),
                    None => ", priced from `[models]`".into(),
                }
            )),
            None => Clause::Fail("nothing to total".into()),
        },
    );

    // ------------------------------------------------------- the resume half
    //
    // The resume spelling is the one clause of the launch half whose failure
    // is silent in production, and the static clause above only proves the
    // rewrite *changes* the argv — not that today's binary still reads what it
    // is rewritten to. Two of the rows were settled by hand against one
    // version of each binary, and a version that changes its grammar would
    // show up first as a person's run failing an hour in, unless something
    // re-asks. So the same session gets a second turn, composed exactly as a
    // lane's second turn is, and the *store* is asked whether it grew rather
    // than the model whether it remembers — a model's recall is obedience,
    // and the transcript the accounting row names is evidence.
    let first_path = crate::usage::session_path(&args.kind, &session);
    let first_turns = crate::usage::harvest(&args.kind, &session).map_or(0, |h| h.turns);
    let resume_argv = adapter
        .headless_args(&rendered_args, true)
        .context("this kind has no headless row")?;
    println!("note  and a second turn, resumed, to check the resume spelling");
    let resumed = run_live_turn(adapter.program(), &resume_argv, RESUME_PROMPT, &dir, env)?;

    report(
        "a resumed turn ran",
        match (resumed.status.success(), resumed.timed_out) {
            (true, _) => Clause::Ok(first_line(&resumed.stdout).to_string()),
            (false, true) => Clause::Fail(format!(
                "`{}` was killed after {}s on the resume spelling",
                args.kind,
                LIVE_TURN_TIMEOUT.as_secs()
            )),
            (false, false) => Clause::Fail(format!(
                "`{}` exited {} on the resume spelling — {}",
                args.kind,
                resumed.status,
                first_line(&resumed.stderr)
            )),
        },
    );

    // Only asked of a turn that ran: a turn that did not has nothing to have
    // landed anywhere, and a second Fail on the same cause would count one
    // defect twice.
    if resumed.status.success() {
        let same_store = crate::usage::session_path(&args.kind, &session) == first_path;
        let turns = crate::usage::harvest(&args.kind, &session).map_or(0, |h| h.turns);
        report(
            "the resumed turn continued the same session",
            match (same_store, turns > first_turns) {
                (true, true) => {
                    Clause::Ok(format!("{first_turns} turn(s) grew to {turns}, one store"))
                }
                // For a kind that pins by home this is a second transcript
                // appearing beside the first; for any kind it is a lane's
                // second turn answering with none of the context the question
                // came from.
                (false, _) => Clause::Fail(
                    "the readings now resolve a different store — the second turn opened a \
                     session of its own instead of continuing the first"
                        .into(),
                ),
                (true, false) => Clause::Fail(format!(
                    "the store the accounting row reads still holds {first_turns} turn(s), so \
                     the second turn landed somewhere spoolway cannot see"
                )),
            },
        );
    }

    println!();
    match failed {
        0 => {
            println!(
                "{} meets the accounting half against a real session — one turn, and one resumed.",
                args.kind
            );
            Ok(())
        }
        n => bail!(
            "{n} reading(s) failed against a real session — what `{}` did does not match its row",
            args.kind
        ),
    }
}

/// A model for a live turn, from the project this was run in.
///
/// spoolway names no model of its own — that refusal is deliberate and stated
/// in `docs/agents.md` — so the only model this may reach for is one the
/// project already named for this kind. The shipped placeholder is skipped: it
/// satisfies every check that asks whether a step names something, and is not
/// a model.
fn live_model(kind: &str, project: Option<(&Repo, &Pipelines)>) -> Option<String> {
    let (repo, pipelines) = project?;
    pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .filter(|step| step.kind() == StepKind::Agent)
        .find_map(|step| {
            let profile = repo.config.agent(step.agent.as_ref()?).ok()?;
            if profile.kind != kind {
                return None;
            }
            let model = step.model.clone()?;
            (!model.trim().is_empty() && model != crate::models::PLACEHOLDER).then_some(model)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--json` prints a JSON array, one object per kind, rather than the
    /// fixed-width table — this is the whole of what `agent_list` promises a
    /// script, so it is enough to check the shape has one row per adapter and
    /// that a real kind's fields read as expected, without pinning every one.
    #[test]
    fn agent_list_json_is_an_array_with_one_row_per_kind() {
        let rows: Vec<(&crate::agent::Adapter, Option<String>)> = crate::agent::ADAPTERS
            .iter()
            .map(|adapter| (adapter, which(adapter.program())))
            .collect();

        let payload = agent_list_json(&rows);
        assert_eq!(payload.len(), crate::agent::ADAPTERS.len());

        let pi = payload
            .iter()
            .find(|row| row["kind"] == "pi")
            .expect("`pi` is one of the adapters");
        assert_eq!(pi["launches"], true);
        assert!(pi["meters"].is_boolean());
        assert!(pi["binary"].is_string() || pi["binary"].is_null());
    }

    /// `--json` and `--live` both ask to change how this reports, and
    /// combining them would mean folding a turn-by-turn narrative into one
    /// JSON payload — out of scope here, so the pair is refused outright
    /// rather than one of them silently losing.
    #[test]
    fn json_and_live_are_refused_together() {
        let args = AgentVerifyArgs {
            kind: "pi".to_string(),
            live: true,
            model: None,
        };
        let err = agent_verify(&args, None, true).unwrap_err();
        assert!(format!("{err:#}").contains("--live"));
    }

    /// The live check's scratch tree is taken back however the check ends.
    ///
    /// `agent_verify_live` itself needs a real agent binary and a paid session,
    /// so what is pinned here is the guard that does the taking back. It went
    /// missing for long enough to leave 531 trees on one machine, and nothing
    /// in the check's own output would ever have said so.
    #[test]
    fn the_live_checks_scratch_tree_goes_when_the_check_does() {
        let dir = crate::scratch::root("agent-verify-scratch");
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        std::fs::write(dir.join("deep/prompt.md"), "You are answering").unwrap();
        // The per-session agent home a kind that pins by home would get —
        // the guard has to take this back too (review finding 62).
        let home = crate::scratch::root("agent-verify-scratch-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("auth.json"), "{}").unwrap();

        {
            let _scratch = ScratchTree {
                dir: dir.clone(),
                home: Some(home.clone()),
            };
            assert!(dir.exists(), "the guard must not delete it early");
        }

        assert!(
            !dir.exists(),
            "the scratch tree outlived its guard: {}",
            dir.display()
        );
        assert!(
            !home.exists(),
            "the per-session home outlived its guard: {}",
            home.display()
        );
    }

    /// And on the way out of a failure, which is the path that matters — a
    /// check that bails is the one a person runs again and again.
    #[test]
    fn the_scratch_tree_goes_on_an_early_return_too() {
        let dir = crate::scratch::root("agent-verify-scratch-bail");
        std::fs::create_dir_all(&dir).unwrap();

        fn bails(dir: &std::path::Path) -> Result<()> {
            let _scratch = ScratchTree {
                dir: dir.to_path_buf(),
                home: None,
            };
            bail!("the reading did not match its row")
        }

        assert!(bails(&dir).is_err());
        assert!(
            !dir.exists(),
            "a check that bailed kept its tree: {}",
            dir.display()
        );
    }

    fn write_claude_json(home: &std::path::Path, body: &str) {
        std::fs::write(home.join(".claude.json"), body).unwrap();
    }

    /// A kind with no [`crate::agent::Adapter::quota`] row at all — `pi`
    /// today — is reported rather than skipped, and never fails the command.
    #[test]
    fn quota_clause_on_a_kind_with_no_probe_warns_it_is_never_parked() {
        let adapter = crate::agent::adapter("pi").expect("pi is a real adapter");
        match quota_clause(adapter) {
            Clause::Warn(note, _) => assert!(
                note.contains("no probe established"),
                "unexpected note: {note}"
            ),
            _ => panic!("a kind with no quota row must warn, not ok or fail"),
        }
    }

    /// A probe row whose file does not exist yet is `Unreadable`, reported
    /// the same way — a warning, never a fail.
    #[test]
    fn quota_clause_on_an_unreadable_file_warns() {
        let home = crate::scratch::root("agent-verify-quota-unreadable");
        std::fs::create_dir_all(&home).unwrap();
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("claude").expect("claude is a real adapter");
            match quota_clause(adapter) {
                Clause::Warn(note, _) => {
                    assert!(
                        note.contains("could not be read"),
                        "unexpected note: {note}"
                    )
                }
                _ => panic!("an unreadable probe must warn, not ok or fail"),
            }
        });
    }

    /// A file that exists but does not parse the cache shape this reader
    /// expects is `Unparseable`, named as such rather than read as an
    /// absent window.
    #[test]
    fn quota_clause_on_an_unparseable_file_warns_it_did_not_parse() {
        let home = crate::scratch::root("agent-verify-quota-unparseable");
        std::fs::create_dir_all(&home).unwrap();
        write_claude_json(&home, "not json at all");
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("claude").expect("claude is a real adapter");
            match quota_clause(adapter) {
                Clause::Warn(note, _) => {
                    assert!(note.contains("did not parse"), "unexpected note: {note}")
                }
                _ => panic!("an unparseable probe must warn, not ok or fail"),
            }
        });
    }

    /// A reading fetched too long ago to trust is reported as stale, treated
    /// as absent rather than acted on.
    #[test]
    fn quota_clause_on_a_stale_reading_warns_it_is_treated_as_absent() {
        let home = crate::scratch::root("agent-verify-quota-stale");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = (chrono::Utc::now() - chrono::Duration::hours(6)).timestamp_millis();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 90, "resets_at": "2099-01-01T00:00:00Z"}},
                    "seven_day": {{"utilization": 90, "resets_at": "2099-01-01T00:00:00Z"}}
                }}}}"#
            ),
        );
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("claude").expect("claude is a real adapter");
            match quota_clause(adapter) {
                Clause::Warn(note, _) => assert!(note.contains("stale"), "unexpected note: {note}"),
                _ => panic!("a stale probe must warn, not ok or fail"),
            }
        });
    }

    /// A fresh, well-formed reading is `Ok`, naming both windows' percentage
    /// and reset — the mockup's own two-window line. Both resets are years
    /// out, so both render `MM-DD HH:MM`, no year — this is the mockup's own
    /// `resets 09-11 04:00` shape, distinct from `format_instant`'s own
    /// cross-day shape (which does carry a year) since this display names
    /// no park for `parked_window` to pin a longer one against.
    ///
    /// The expected text is derived from the same instant independently
    /// rather than hand-typed, so the assertion holds whatever local
    /// timezone the test happens to run under.
    #[test]
    fn quota_clause_on_a_fresh_reading_reports_both_windows() {
        let home = crate::scratch::root("agent-verify-quota-fresh");
        std::fs::create_dir_all(&home).unwrap();
        let fetched_at = chrono::Utc::now().timestamp_millis();
        let five_hour_resets: chrono::DateTime<chrono::Utc> =
            "2099-01-01T00:00:00Z".parse().unwrap();
        let seven_day_resets: chrono::DateTime<chrono::Utc> =
            "2099-02-01T04:00:00Z".parse().unwrap();
        write_claude_json(
            &home,
            &format!(
                r#"{{"cachedUsageUtilization": {{
                    "fetchedAtMs": {fetched_at},
                    "five_hour": {{"utilization": 61, "resets_at": "{}"}},
                    "seven_day": {{"utilization": 16, "resets_at": "{}"}}
                }}}}"#,
                five_hour_resets.to_rfc3339(),
                seven_day_resets.to_rfc3339(),
            ),
        );
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("claude").expect("claude is a real adapter");
            let five_hour_display = five_hour_resets
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M");
            let seven_day_display = seven_day_resets
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M");
            match quota_clause(adapter) {
                Clause::Ok(note) => {
                    assert!(
                        note.contains(&format!("five_hour 61% resets {five_hour_display}")),
                        "unexpected note: {note}"
                    );
                    assert!(
                        note.contains(&format!("seven_day 16% resets {seven_day_display}")),
                        "unexpected note: {note}"
                    );
                    assert!(!note.contains("2099-"), "must not carry a year: {note}");
                }
                _ => panic!("a fresh reading must report ok with both windows"),
            }
        });
    }

    /// A rollout under the managed lane home `spoolway agent verify`'s codex
    /// quota clause actually reads —
    /// `<home>/.local/state/spoolway/codex/<session>/sessions/**` — never
    /// `~/.codex`. See `crate::quota::tests::write_managed_codex_rollout`,
    /// this command's own copy of the same fixture shape.
    fn write_codex_rollout(home: &std::path::Path, timestamp: &str, rate_limits: &str) {
        let dir = home.join(".local/state/spoolway/codex/fixture-session/sessions/2026/09/05");
        std::fs::create_dir_all(&dir).unwrap();
        let line = format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":{rate_limits}}}}}"#
        );
        // One record per physical line, the way a real rollout is — the
        // multi-line `rate_limits` constants below are kept readable, not
        // written that way for real, so their embedded newlines are folded
        // out first.
        std::fs::write(dir.join("rollout-fixture.jsonl"), line.replace('\n', "")).unwrap();
    }

    const REAL_SHAPED_RATE_LIMITS: &str = r#"{"limit_id":"codex","limit_name":null,
        "primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1788611977},
        "secondary":{"used_percent":2.0,"window_minutes":10080,"resets_at":1789151593},
        "credits":{"has_credits":false,"unlimited":false,"balance":"0"},
        "individual_limit":null,"spend_control_reached":null,"plan_type":"plus",
        "rate_limit_reached_type":null}"#;

    const NULL_RATE_LIMITS: &str = r#"{"limit_id":"codex","limit_name":null,"primary":null,
        "secondary":null,"credits":null,"individual_limit":null,
        "spend_control_reached":null,"plan_type":null,"rate_limit_reached_type":null}"#;

    /// codex's own row, once signed in with ChatGPT: `primary`/`secondary`,
    /// not claude's `five_hour`/`seven_day` labels, and a countdown rather
    /// than a clock — the mockup's own `resets in 2h11m` shape. The
    /// `timestamp` itself is `Utc::now()`, so this stays fresh however long
    /// after that real capture the suite happens to run.
    #[test]
    fn quota_clause_on_a_fresh_codex_reading_reports_primary_and_secondary() {
        let home = crate::scratch::root("agent-verify-quota-codex-fresh");
        std::fs::create_dir_all(&home).unwrap();
        write_codex_rollout(
            &home,
            &chrono::Utc::now().to_rfc3339(),
            REAL_SHAPED_RATE_LIMITS,
        );
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("codex").expect("codex is a real adapter");
            match quota_clause(adapter) {
                Clause::Ok(note) => {
                    assert!(
                        note.contains("primary 5% resets in"),
                        "unexpected note: {note}"
                    );
                    assert!(
                        note.contains("secondary 2% resets in"),
                        "unexpected note: {note}"
                    );
                    assert!(
                        note.contains("newest rollout under the lane's CODEX_HOME"),
                        "unexpected note: {note}"
                    );
                    assert!(
                        !note.contains("five_hour") && !note.contains("seven_day"),
                        "codex's row must not borrow claude's window labels: {note}"
                    );
                }
                _ => panic!("a fresh codex reading must report ok with both windows"),
            }
        });
    }

    /// The mockup's own codex row, pinned exactly rather than by substring —
    /// `render_quota_row` is what `spoolway agent verify` actually prints, so
    /// this is the line a person reading the command's output really sees.
    /// Reset times are relative durations, not a clock, so only their
    /// `resets in <duration>` shape is checked — the exact figure depends on
    /// how much of the fixed window has run out by the time this executes.
    #[test]
    fn agent_verify_prints_codexs_quota_row_exactly_as_the_mockup_draws_it() {
        let home = crate::scratch::root("agent-verify-quota-codex-exact");
        std::fs::create_dir_all(&home).unwrap();
        write_codex_rollout(
            &home,
            &chrono::Utc::now().to_rfc3339(),
            REAL_SHAPED_RATE_LIMITS,
        );
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("codex").expect("codex is a real adapter");
            let clause = quota_clause(adapter);
            let rendered = render_quota_row("codex", &clause);
            let mut lines = rendered.lines();
            assert_eq!(
                lines.next().unwrap(),
                "codex    quota  newest rollout under the lane's CODEX_HOME,"
            );
            assert_eq!(
                lines.next().unwrap(),
                "                last token_count event's rate_limits"
            );
            let third = lines.next().unwrap();
            assert!(
                third.starts_with("                primary 5% resets in ")
                    && third.contains(" · secondary 2% resets in "),
                "unexpected third line: {third}"
            );
        });
    }

    /// codex's own negative case — a session settled against a local
    /// endpoint or a bare API key — is reported rather than misread as a
    /// parse failure, and never fails the command.
    #[test]
    fn quota_clause_on_a_codex_session_with_null_rate_limits_warns_not_signed_in() {
        let home = crate::scratch::root("agent-verify-quota-codex-null");
        std::fs::create_dir_all(&home).unwrap();
        write_codex_rollout(&home, "2026-09-05T07:51:22.019Z", NULL_RATE_LIMITS);
        crate::platform::test_home::with_home(&home, || {
            let adapter = crate::agent::adapter("codex").expect("codex is a real adapter");
            match quota_clause(adapter) {
                Clause::Warn(note, _) => assert!(
                    note.contains("not signed in with ChatGPT"),
                    "unexpected note: {note}"
                ),
                _ => panic!("a null codex reading must warn, not ok or fail"),
            }
        });
    }

    /// The mockup's own two rows — `claude` and `pi`, both a multi-line
    /// `Ok`/`Warn` — line up their `quota` label at the same column however
    /// differently long the two kind names are, and each continuation line
    /// lands under the note rather than under the kind name.
    #[test]
    fn render_quota_row_lines_up_kind_names_and_indents_continuation() {
        let ok = Clause::Ok(
            "~/.claude.json cachedUsageUtilization\nfive_hour 61% resets 14:00 · seven_day \
             16% resets 09-11 04:00"
                .to_string(),
        );
        let warn = Clause::Warn(
            "no probe established — lanes of this kind are never parked\nfor quota, and its \
             usage limit is not detected either"
                .to_string(),
            Vec::new(),
        );
        let claude_row = render_quota_row("claude", &ok);
        let pi_row = render_quota_row("pi", &warn);

        let mut claude_lines = claude_row.lines();
        let claude_first = claude_lines.next().unwrap();
        let claude_second = claude_lines.next().expect("a continuation line");
        let mut pi_lines = pi_row.lines();
        let pi_first = pi_lines.next().unwrap();
        let pi_second = pi_lines.next().expect("a continuation line");

        assert_eq!(
            claude_first,
            "claude   quota  ~/.claude.json cachedUsageUtilization"
        );
        assert_eq!(
            claude_second,
            "                five_hour 61% resets 14:00 · seven_day 16% resets 09-11 04:00"
        );
        assert_eq!(
            pi_first,
            "pi       quota  no probe established — lanes of this kind are never parked"
        );
        assert_eq!(
            pi_second,
            "                for quota, and its usage limit is not detected either"
        );

        // Both rows' `quota` label starts at the same column, however
        // differently long "claude" and "pi" are.
        assert_eq!(claude_first.find("quota"), pi_first.find("quota"));
    }
}
