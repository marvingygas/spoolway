//! The pane-facing command: `spoolway lane` — reads a lane, answers it, or
//! attaches to it, depending on which flag was given.
//!
//! `attach`, `answer` and `logs` below keep the bodies and the error text
//! they had as their own clap commands; only [`lane_cmd`] and the argument
//! structs it builds for them are new. That split stays because the three
//! questions really are different functions — `attach` execs into a child
//! process and never returns on success, `answer` needs a live `Mux`, `logs`
//! makes its own — and folding them into one body would tangle three control
//! flows for no reader's benefit.

use super::*;

/// Arguments for [`logs`], [`answer`] and [`attach`] — no longer clap
/// structs themselves, since `spoolway lane` is the one shape clap parses
/// now. [`lane_cmd`] builds one of these from a [`crate::cli::LaneArgs`] and
/// hands it to whichever of the three answers the flags asked for.
pub struct LogsArgs {
    pub lane: Option<String>,
    pub lines: usize,
}

pub struct AnswerArgs {
    pub lane: String,
    pub message: Vec<String>,
}

pub struct AttachArgs {
    pub lane: String,
}

/// `spoolway lane`: which of the three questions the flags ask.
///
/// Not named `lane`: `commands::testutil::lane` already is, building a test
/// fixture rather than running a command, and a module that globs both in
/// (as `cost`'s tests do) cannot tell them apart.
///
/// At most one of `message` and `attach` can be set — clap already refused
/// the combination, and refused `--lines` alongside either, at parse time —
/// so this is a plain three-way branch rather than a validation.
pub fn lane_cmd(
    repo: &Repo,
    pipelines: &Pipelines,
    mux: &dyn Mux,
    args: &crate::cli::LaneArgs,
    in_lane: bool,
    json: bool,
) -> Result<()> {
    // `requires = "lane"` on both flags in `LaneArgs` means clap has already
    // refused `-m` or `--attach` with no lane, so `expect` here is
    // unreachable rather than a real fallibility. `--json` alongside either
    // is silently ignored, the same as `--json` alongside a subcommand with
    // nothing to render as JSON: `--attach` execs into another process and
    // `-m` sends a message, and neither prints a reading there is a second
    // way to say.
    if args.attach {
        let lane = args
            .lane
            .clone()
            .expect("clap requires a lane with --attach");
        return attach(repo, pipelines, mux, &AttachArgs { lane });
    }
    if let Some(message) = &args.message {
        refuse_from_lane("a lane is answered", in_lane)?;
        let lane = args.lane.clone().expect("clap requires a lane with -m");
        // A one-element Vec, not a flattened String: `answer`'s body — kept
        // byte-for-byte per the acceptance criteria — still does
        // `args.message.join(" ")`, and a single element joins back to
        // itself, so this is what lets that body stay untouched.
        return answer(
            repo,
            pipelines,
            mux,
            &AnswerArgs {
                lane,
                message: vec![message.clone()],
            },
        );
    }
    logs(
        repo,
        pipelines,
        &LogsArgs {
            lane: args.lane.clone(),
            lines: args.lines,
        },
        json,
    )
}

/// A person's answer to a lane that ended its turn on a question.
///
/// Open a lane's session in this terminal, with its whole conversation.
///
/// The interactive counterpart of [`answer`]. Under a resident backend the
/// session is already open in its pane, and a second client on one session is
/// a corruption nobody asked for — so there this only says where the pane is.
/// Headless, the lane's own session is reopened by the same resume flags a
/// later turn would use, in the same working directory the lane ran in,
/// because at least one kind keys its session store on that directory.
pub fn attach(repo: &Repo, pipelines: &Pipelines, mux: &dyn Mux, args: &AttachArgs) -> Result<()> {
    let lane = args.lane.trim();
    let Some((_, task_id)) = crate::mux::parse_lane_name(lane, &pipelines.all_step_ids()) else {
        bail!(
            "`{lane}` is not a spoolway lane — a lane is named `<task> · <step>`, and \
             a dispatch run's board names the waiting ones"
        );
    };

    if mux.resident_while_waiting()
        && let Ok(lanes) = mux.list_lanes()
        && let Some(live) = lanes.iter().find(|l| l.name == lane)
    {
        println!(
            "`{lane}` is resident in pane {} (workspace {}) — its session is already \
                     open there; type into it rather than opening it twice.",
            live.pane_id, live.workspace_id
        );
        // The workspace id is a tmux session id, and `attach -t` on an id is
        // the one gesture a person who knows no tmux needs handed to them.
        if mux.name() == "tmux" {
            println!("    tmux attach -t '{}'", live.workspace_id);
        }
        return Ok(());
    }

    let (kind, session) = crate::dispatch::lane_session(repo, lane).with_context(|| {
        format!(
            "no session recorded for `{lane}` — nothing to reopen. \
             `spoolway lane {lane}` still has its output"
        )
    })?;
    let adapter = crate::agent::adapter(&kind)
        .with_context(|| format!("lane `{lane}` ran an unknown agent kind `{kind}`"))?;
    let argv = adapter.attach_args(&session).with_context(|| {
        format!("`{kind}` has no established resume flags, so its session cannot be reopened")
    })?;

    // The lane's own working directory, while it still exists: at least one
    // kind stores sessions per directory, and resuming from anywhere else
    // would find nothing.
    let cwd = repo
        .task(task_id)
        .ok()
        .and_then(|task| task.front.worktree_path.clone())
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| repo.root.clone());

    eprintln!("opening `{lane}` ({kind}, session {session})…");
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(&cwd)
        // For a kind whose session is pinned by a home rather than by an id,
        // this is what makes the argv above mean *this* session. Empty for
        // every other kind.
        .envs(adapter.session_env(&session));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Become the agent: same terminal, same stdio, nothing left behind.
        Err(command.exec()).with_context(|| format!("starting `{}`", argv[0]))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("starting `{}`", argv[0]))?;
        if !status.success() {
            bail!("`{}` exited with {status}", argv[0]);
        }
        Ok(())
    }
}

/// The counterpart of typing into the lane's pane, for the backend that has no
/// pane. This is the person the gate exists for, so the only checks here are
/// the ones that keep the message from going somewhere it cannot be read.
pub fn answer(repo: &Repo, pipelines: &Pipelines, mux: &dyn Mux, args: &AnswerArgs) -> Result<()> {
    let message = args.message.join(" ");
    let message = message.trim();
    if message.is_empty() {
        bail!("an answer with no words in it is not one");
    }

    let lane_name = args.lane.trim();
    let (_, task_id) = crate::mux::parse_lane_name(lane_name, &pipelines.all_step_ids())
        .with_context(|| {
            format!(
                "`{lane_name}` is not a lane name — lanes are `<task> · <step>`, and \
                 `spoolway lane` lists the ones that exist"
            )
        })?;
    let task = repo.task(task_id)?;

    let lanes = mux.list_lanes()?;
    let lane = lanes
        .iter()
        .find(|lane| lane.name == lane_name)
        .with_context(|| format!("no live lane `{lane_name}` — nothing there to answer"))?;

    // The same ownership rule everything else works to: a name alone does not
    // say whose session it is.
    let ours = lane.cwd == repo.root || Some(&lane.cwd) == task.front.worktree_path.as_ref();
    if !ours {
        bail!(
            "lane `{lane_name}` is working in {}, which is not this project",
            lane.cwd.display()
        );
    }

    if lane.status.is_busy() {
        bail!(
            "lane `{lane_name}` is `{}` — it has not finished its turn, so there is nothing \
             to answer yet. `spoolway lane {lane_name}` shows where it is.",
            lane.status
        );
    }

    mux.prompt(lane_name, message)?;
    println!("{lane_name}: answered — `spoolway lane {lane_name}` follows what it does with it");
    Ok(())
}

/// Read a lane, without a model and without attaching to anything.
///
/// This says what a lane wrote, the way looking at a pane costs nothing.
/// Headless it is the only way to watch a lane at all.
pub fn logs(repo: &Repo, pipelines: &Pipelines, args: &LogsArgs, json: bool) -> Result<()> {
    let mux = crate::mux::backend(repo);

    let Some(lane) = args.lane.clone() else {
        let lanes = mux.list_lanes()?;
        if json {
            let rows: Vec<serde_json::Value> = lanes
                .iter()
                .map(|lane| serde_json::json!({"name": lane.name, "status": lane.status.to_string()}))
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        if lanes.is_empty() {
            println!("no lanes are running.");
            return Ok(());
        }
        for lane in lanes {
            println!("  {:<24} {}", lane.name, lane.status);
        }
        return Ok(());
    };

    // Named rather than guessed at, and checked against the pipeline:
    // `<task> · <step>` is what makes a lane spoolway's, and a name that is
    // not one is a person's typo far more often than it is a lane nobody
    // listed.
    if crate::mux::parse_lane_name(&lane, &pipelines.all_step_ids()).is_none() {
        bail!(
            "`{lane}` is not a lane name — lanes are `<task> · <step>`, and `spoolway lane` with \
             no argument lists the ones that exist"
        );
    }

    // A command step's output is never on a pane, and the pane it would be
    // read from is actively misleading: a `run:` step attaches no agent, so
    // what is on screen is residue from whichever agent step ran there last.
    // A lane an hour into a 45-minute suite and a lane whose agent died look
    // identical from there, which is what made a working lane read as a dead
    // one. The wrapper has been writing both streams to a file the whole time,
    // so this reads that instead, and reads it first.
    if let Some(text) = command_log(repo, &lane, args.lines) {
        return print_lane_text(&lane, &text, json);
    }

    // A live pane is still the pane: it is where the lane is actually
    // writing, and reading it costs nothing whether the lane is a staffed
    // `blocked` step's or any other. Only once that fails — no pane, because
    // the lane has already settled and, on an unattended run, its pane was
    // closed before this command could be typed — is there anything to fall
    // back to.
    let text = match mux.read(&lane, args.lines) {
        Ok(text) => text,
        Err(err) => {
            let Some((kind, session)) = crate::dispatch::lane_session(repo, &lane) else {
                return Err(err);
            };
            crate::usage::transcript_tail(&kind, &session, args.lines).ok_or(err)?
        }
    };
    print_lane_text(&lane, &text, json)
}

/// One lane's read-out, as `--json` or as the plain text it always was —
/// shared by both sources `logs` reads from, a command step's log and a
/// live pane, since a JSON consumer wants the same shape whichever one
/// answered.
fn print_lane_text(lane: &str, text: &str, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"lane": lane, "text": text}))?
        );
        return Ok(());
    }
    match text.trim().is_empty() {
        true => println!("`{lane}` has written nothing yet."),
        false => println!("{text}"),
    }
    Ok(())
}

/// The tail of a command step's log, with a line saying which run it is and
/// where the whole of it lives.
///
/// `None` when this lane has no command run at all, which is every agent lane
/// — the caller then reads the pane as it always did. A lane key is a command
/// run's key, so no lookup beyond the file is needed.
fn command_log(repo: &Repo, lane: &str, lines: usize) -> Option<String> {
    let runs = crate::command_step::Runs::new(&repo.root);
    let path = runs.log_path(lane);
    let body = std::fs::read_to_string(&path).ok()?;

    let tail: Vec<&str> = body.lines().rev().take(lines).collect();
    let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");

    let state = match runs.state(lane) {
        crate::command_step::RunState::Running => match runs.elapsed(lane) {
            Some(for_) => format!(
                "running for {}",
                crate::config::human_duration::format(for_)
            ),
            None => "running".to_string(),
        },
        crate::command_step::RunState::Exited(code) => format!("exited {code}"),
        crate::command_step::RunState::Interrupted => {
            "interrupted without an exit code — it will be run again".to_string()
        }
        crate::command_step::RunState::Fresh => "over, and its run has been forgotten".to_string(),
    };
    Some(format!(
        "`{lane}` is a command step, {state} — {}\n\n{tail}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// A lane can no longer answer for itself: `spoolway lane -m` refuses
    /// before it ever touches a `Mux`, which is why a real `Headless` backend
    /// is enough here — nothing about it is expected to run.
    #[test]
    fn lane_m_refuses_from_inside_a_lanes_own_environment() {
        let repo = fixture("lane-m-in-lane");
        let pipelines = Pipelines::builtin();
        let headless =
            crate::headless::Headless::new(&repo.root, &repo.config.dispatch, repo.headless_dir());

        let err = lane_cmd(
            &repo,
            &pipelines,
            &headless,
            &crate::cli::LaneArgs {
                lane: Some("task · step".into()),
                message: Some("go ahead".into()),
                attach: false,
                lines: 200,
            },
            true,
            false,
        )
        .expect_err("a lane must not be able to answer another lane for itself");
        let said = format!("{err:#}");
        assert!(said.contains("lane's own environment"), "{said}");
    }
}
