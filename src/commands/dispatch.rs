//! `spoolway dispatch`: the run loop around [`crate::dispatch`], and stopping one.

use anyhow::anyhow;

use super::*;
use crate::dispatch::{RESTART_MAX, RESTART_WINDOW};
use crate::screen::PollableRead;

/// An empty queue is an ordinary ending: nothing was there to dispatch, and
/// running again once something is queued is exactly what a person or a
/// script should do next. Only when no job is enabled — an enabled job keeps
/// the run resident on an empty queue instead, so it is alive when the
/// job's window comes round.
pub const EXIT_EMPTY_QUEUE: i32 = 3;

/// Another dispatcher already holds the repo lock. Ordinary too — the
/// caller's own next pass, or the one already running, will pick up
/// whatever it queued — but distinct from an empty queue, since a caller
/// restarting a dispatcher forever needs to be able to tell the two apart.
pub const EXIT_ALREADY_RUNNING: i32 = 4;

/// One start that could not run at all, refused by the restart guard,
/// naming the count, the last reason and the way out.
///
/// A distinct error type, downcast at the top of [`crate::main`], because a
/// restart storm needs its own exit code — 5 — where every other failure
/// this command can have exits 1.
#[derive(Debug)]
pub struct RestartsRefused {
    pub count: u32,
    pub reason: String,
}

impl std::fmt::Display for RestartsRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to start: {} starts in a row could not run at all, the last because {}. \
             Fix the reason above, or run `spoolway dispatch --force` to start anyway.",
            self.count, self.reason
        )
    }
}

impl std::error::Error for RestartsRefused {}

/// Run the pipeline: one pass, or a loop on the configured interval.
///
/// Returns the code the process should exit with, rather than `()`, so a
/// caller restarting this in a tight loop against a repo that cannot run
/// can tell an empty queue (3), a lock already held (4), and a run that
/// dispatched and stopped on its own (0) apart. Every genuine error is
/// still `Err`, including a restart storm's own [`RestartsRefused`] — see
/// `crate::main` for where each of these becomes a process exit code.
pub fn dispatch(repo: &Repo, pipelines: &Pipelines, args: &DispatchArgs) -> Result<i32> {
    // The mode is settled here, once, and written into the lock — every lane's
    // own `spoolway report` reads it back from there, so a `--unattended` typed
    // on this command line reaches the processes that actually decide what a
    // block does.
    let unattended = args.unattended(&repo.config);

    // An unattended run staffs `blocked` instead of parking a task for a
    // person, on the model `unattended.blocked_model` names — and a blank
    // one means no lane could ever launch there. Refused up front, ahead of
    // the lock and the queue: a config problem is true whether or not
    // anything is queued right now, and starting the run to let the first
    // block discover this is a run that has to be found and stopped by
    // hand, at whatever hour the block happened to land.
    //
    // `unattended` here can come from either of two places — the
    // `--unattended` flag on this command line, or `unattended.enabled` in
    // config.toml — and only one of them is actually true right now. Naming
    // the wrong one would tell somebody to flip a switch that is already off.
    if unattended && repo.config.unattended.blocked_model.trim().is_empty() {
        let (cause, fix) = if args.unattended {
            ("the --unattended flag is set", "drop --unattended")
        } else {
            ("unattended.enabled is on", "turn unattended.enabled off")
        };
        bail!(
            "{cause} and unattended.blocked_model is blank — an unattended run has nobody to \
             clear a block, so `blocked` has to be staffed. Set unattended.blocked_model, or \
             {fix}."
        );
    }

    // The guard for a caller restarting this in a tight loop against a repo
    // that cannot run at all: refused ahead of the checks below, since it is
    // exactly the endings those checks produce that count towards it.
    //
    // `--force` clears the count rather than merely stepping past this one
    // refusal, on the same reasoning as a start that actually runs, below:
    // whatever was building is over, one way or another, and the next
    // restart storm starts its own count from zero.
    if args.force {
        crate::lock::Restarts::clear(&repo.restarts_file())?;
    } else if let Some((count, reason)) =
        crate::lock::Restarts::status(&repo.restarts_file(), RESTART_MAX, RESTART_WINDOW)?
    {
        return Err(anyhow!(RestartsRefused { count, reason }));
    }

    // One dispatcher serves the whole repo, whichever worktree it was started
    // in: the queue belongs to the main checkout, and a task carries the branch
    // it was queued on. So a dispatcher that is already running will pick up
    // what was just queued from another plan's worktree on its next pass —
    // there is nothing to start, and saying so is not a failure.
    if let Some(pid) = crate::lock::Lock::holder(&repo.lock_file())? {
        // Could not run — counted so the guard above can eventually refuse a
        // caller that keeps restarting into the same held lock.
        crate::lock::Restarts::note_refusal(
            &repo.restarts_file(),
            &format!("a dispatcher is already running for this repo (pid {pid})"),
            RESTART_WINDOW,
        )?;
        watch(repo, pipelines, args)?;
        return Ok(EXIT_ALREADY_RUNNING);
    }

    // Whether the run has already said, this spell of empty queue, that a job
    // is keeping it resident. Reset every time the queue is not empty, so the
    // next drain says it again.
    let mut idle_announced = false;

    // Nothing queued is nothing to dispatch — unless a job is enabled, which
    // keeps the run resident so it is alive when that job's window comes
    // round. Without the guard the loop would sit there printing "nothing to
    // do" until somebody noticed, and a dispatcher started in the wrong
    // project looks exactly like one with no work yet.
    //
    // Not counted: a repo with nothing to do is not a storm, and restarting
    // into an empty queue forever is a caller's own choice to make, not
    // something this guard has any business refusing.
    if repo.tasks()?.is_empty() {
        if crate::jobs::enabled_count(repo) == 0 {
            println!("nothing is queued, so there is nothing to dispatch.");
            println!("  spoolway queue add --from <path>");
            return Ok(EXIT_EMPTY_QUEUE);
        }
        print_staying_up(&crate::jobs::staying_up(repo));
        idle_announced = true;
    }

    // A start that gets this far can actually run. Whatever the guard above
    // was counting, it was counting starts that could not — this is not one
    // of them, so the slate is clean again.
    crate::lock::Restarts::clear(&repo.restarts_file())?;

    let mux = crate::mux::backend(repo);
    if !mux.is_available() {
        bail!("{}", mux.unavailable());
    }

    // The three things that certainly break a run, refused here rather than
    // left for a lane to discover mid-turn: no git identity where something
    // is about to commit, a stale index lock that fails every git write
    // `git status` itself is blind to, and a backend that cannot actually
    // open this checkout. Ahead of the lock, same as the mux check just
    // above — a config problem is true whether or not anything is queued,
    // and letting the run start only for the first lane to hit it means it
    // has to be found and stopped by hand. `doctor` reports the same three
    // as rows, off the same functions, so the fix here and the fix there
    // never drift apart.
    refuse(check_git_identity(repo, pipelines, &repo.config)).context("refusing to start")?;
    refuse(check_index_lock(repo)).context("refusing to start")?;
    refuse(check_backend_checkout(repo, mux.as_ref())).context("refusing to start")?;

    // Taken for the whole run. Two dispatchers would both see the same task at
    // the same step and both spawn a lane into its worktree.
    let _lock = crate::lock::Lock::acquire(&repo.lock_file(), unattended)?;

    if unattended {
        println!(
            "unattended: blocks resume where they happened, and a `loop` that gives up into \
             `blocked` does not apply. A gated step still parks on `paused` for you."
        );
        match repo.config.unattended.max_output_tokens {
            0 => println!(
                "  no unattended.max_output_tokens is set, so nothing bounds this run in \
                 tokens."
            ),
            ceiling => println!("  stopping once this run has spent {ceiling} output tokens."),
        }
        match repo.config.unattended.max_cost_usd {
            ceiling if ceiling <= 0.0 => println!(
                "  no unattended.max_cost_usd is set, so nothing bounds this run in dollars — \
                 an empty queue or ctrl-c is what ends it if neither ceiling is."
            ),
            ceiling => println!("  stopping once this run has spent ${ceiling:.2}."),
        }
    }

    // Note this project once per run, so `spoolway eval --by --all` can find
    // its ledger later. A project that is dispatched in is a project that spends.
    crate::usage::registry::register(&repo.root);

    // Trimmed once, here, rather than on every append below — see
    // `crate::problem_log::open`. A run that never hits a problem never
    // touches this file at all.
    crate::problem_log::open(repo);

    let interval = match &args.interval {
        Some(text) => crate::config::parse_duration(text).map_err(anyhow::Error::msg)?,
        None => repo.config.dispatch.interval,
    };

    if args.dry_run {
        println!("dry run: nothing will be started, torn down, or written\n");
    }

    // Under herdr this is a no-op: the dispatcher's own pane stays wherever it
    // was started, under both `grouped` and `split` — see `Mux::move_self_into`.
    // It is tmux that still needs this: this is where it moves the caller's
    // own window into the run's shared session.
    //
    // Never for `--dry-run`, which opens nothing and closes nothing. Failure is
    // said out loud and stepped over: a run whose pane could not be moved is
    // still a run, drawing where it was started, and its stop closes the
    // tab behind it as it did before — the sweep asks where this pane
    // actually is rather than assuming the move landed.
    if !args.dry_run {
        match mux
            .dispatch_workspace(&repo.root, true)
            .and_then(|workspace| match workspace {
                Some(id) => {
                    mux.move_self_into(&id)?;
                    // A run in a background tmux server is invisible until
                    // attached to, and the person this backend is for may
                    // never have typed a tmux command — the one they need is
                    // handed over. Only when the board is not about to draw
                    // in that session anyway: a dispatcher started inside
                    // tmux moved there with its pane.
                    if mux.name() == "tmux" && mux.own_workspace().as_deref() != Some(id.as_str()) {
                        println!(
                            "  the run's lanes live in tmux — watch them with: tmux attach -t '{}'",
                            crate::tmux::session_name(&crate::mux::dispatch_workspace_label(
                                &repo.root
                            ))
                        );
                    }
                    Ok(())
                }
                None => Ok(()),
            }) {
            Ok(()) => {}
            Err(err) => println!("  ! could not move this run into its own workspace: {err:#}"),
        }
    }

    // The run watches itself. A resident dispatcher spends almost all of its
    // time waiting for the next pass, and drawing the board through that wait
    // costs the run nothing — where a scrolling log says only what the last
    // pass did, the board says what the whole queue is doing right now.
    //
    // Not for `--dry-run`, which is a person already looking at one pass, and
    // not for `--plain`, which is a person who would rather have the log — a
    // pipe, a CI job, a terminal that mangles the redraw.
    // From here the run holds live lanes, so an interrupted one's spend and
    // launch counter still have to be settled on the way out. Caught rather
    // than left to kill the process where it stands — see
    // `crate::dispatch::Dispatcher::sweep_on_stop`.
    if !args.dry_run {
        crate::platform::stop::catch_interrupt();
    }

    let mut board = match args.dry_run || args.plain {
        true => None,
        false => Some(crate::status::Board::new()),
    };
    let mut out = std::io::stdout();

    loop {
        let mut dispatcher =
            crate::dispatch::Dispatcher::new(repo, pipelines, mux.as_ref(), args.dry_run);

        // Drawn before the pass rather than only after it, so a pass that takes
        // a while is a board saying "pass running" instead of a blank terminal.
        if let Some(board) = board.as_mut() {
            let _ = board.draw(repo, pipelines, crate::status::Phase::Passing, &mut out);
        }

        let mut spent_out = None;
        match dispatcher.pass() {
            Ok(report) => {
                // The ceiling drains rather than kills: the run is over, but not
                // until whatever is mid-turn has had its chance to report. A
                // lane torn down halfway spent its tokens and produced nothing.
                if let Some(note) = &report.ceiling
                    && !report.lanes_live
                {
                    spent_out = Some(note.clone());
                }
                // Appended whether or not a board is drawn: the board no
                // longer carries a pass's trouble at all, so this is the only
                // place any of it is kept. See `crate::problem_log`.
                for problem in &report.problems {
                    crate::problem_log::append(repo, problem);
                }
                // With a board up, what a pass did is already on screen — the
                // rows it moved are the report, and its trouble just went to
                // the log above rather than the board.
                if board.is_none() {
                    for action in &report.actions {
                        println!("  {action}");
                    }
                    for problem in &report.problems {
                        println!("  ! {problem}");
                    }
                    if report.actions.is_empty() && report.problems.is_empty() {
                        // A pass that started nothing because every lane is
                        // still grinding is not an idle pipeline, and saying
                        // "nothing to do" over a working lane is how a wedged
                        // run hides in its own log. `quiet` is the distinction
                        // the pass already drew.
                        println!(
                            "  {}",
                            match report.quiet {
                                true => "nothing to do",
                                false => "lanes still working",
                            }
                        );
                    }
                }
            }
            // A failed pass must not kill the loop: a transient herdr hiccup or
            // a half-written task file should cost one pass, not the run.
            Err(err) => {
                let failure = format!("pass failed: {err:#}");
                crate::problem_log::append(repo, &failure);
                if board.is_none() {
                    println!("  ! {failure}");
                }
            }
        }

        // A dry run is one pass. It archives nothing, so the empty-queue exit
        // below can never fire and a looping dry run would keep reporting the
        // same untouched queue — the second pass has nothing to add to the
        // first.
        if args.dry_run {
            return Ok(0);
        }

        // Spent, and nothing left running to spend more. The queue keeps its
        // place: every task is where its last lane left it, and the next run
        // picks them up from exactly there.
        if let Some(note) = spent_out {
            stop(
                repo,
                pipelines,
                mux.as_ref(),
                board.as_mut(),
                &mut out,
                args,
            )?;
            println!("  {note}");
            println!("  spoolway dispatch    # picks the queue back up where it stands");
            return Ok(0);
        }

        // A task file leaves the queue only when its terminal step archives it,
        // so an empty queue means every task is finished — including any queued
        // from another plan's worktree while this loop was running, since they
        // all arrive in this one queue. A blocked or paused task stays in the
        // queue, and so does the loop: those are waiting on a person, not done.
        // Asked for while the last pass was running. Unwound here rather than
        // in the handler, which may do nothing but set the flag.
        if crate::platform::stop::asked() {
            stop(
                repo,
                pipelines,
                mux.as_ref(),
                board.as_mut(),
                &mut out,
                args,
            )?;
            println!("  stopped.");
            return Ok(0);
        }

        match repo.tasks() {
            Ok(tasks) if tasks.is_empty() => {
                if crate::jobs::enabled_count(repo) == 0 {
                    stop(
                        repo,
                        pipelines,
                        mux.as_ref(),
                        board.as_mut(),
                        &mut out,
                        args,
                    )?;
                    println!("  queue is empty — every task is done. Stopping.");
                    return Ok(0);
                }
                // A job keeps the run resident. Say so once per spell of
                // empty queue — not every pass, and never over a board,
                // which owns the screen — then loop on into the wait.
                if board.is_none() && !idle_announced {
                    println!();
                    print_staying_up(&crate::jobs::staying_up(repo));
                }
                idle_announced = true;
            }
            // A queue that cannot be read is a reason to try again next pass,
            // not to decide the work is finished.
            _ => idle_announced = false,
        }

        match board.as_mut() {
            // The wait, redrawn: the same sleep the plain run takes, cut into
            // frames so the board is current while nothing is happening —
            // and, with a board up, a wait spent listening rather than
            // discarding whatever lands on the terminal it already holds in
            // raw mode.
            //
            // `byte_pending` stands in for the sleep itself rather than
            // beside it: it blocks the kernel's own `poll` for exactly the
            // slice a plain sleep would have taken, so a wait with nothing
            // typed into it costs the loop nothing extra — the same number
            // of redraws, over the same wall clock, as before this read a
            // key at all. A key applies to the board and the loop goes
            // straight back around to redraw it; nothing pressed and this is
            // the old sleep, waited out in full.
            Some(board) => {
                let mut stdin = crate::screen::RawStdin;
                // Set once, the moment stdin is found to have gone away —
                // a closed pipe, or no controlling terminal at all — and
                // never asked again after that. `byte_pending` reports a
                // closed descriptor "ready" exactly as it does a real
                // keystroke, since the read that follows either way returns
                // promptly; without this guard the loop would keep taking
                // that as a key, get `None` back from `read_key` every time,
                // and spin the wait down to nothing for the rest of the run.
                let mut listening = true;
                let until = std::time::Instant::now() + interval;
                while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
                    if crate::platform::stop::asked() {
                        break;
                    }
                    let _ = board.draw(repo, pipelines, crate::status::Phase::Waiting, &mut out);
                    let slice = crate::status::POLL.min(left);
                    if listening && stdin.byte_pending(slice) {
                        match crate::screen::read_key(&mut stdin) {
                            Some(key) => {
                                let _ = board.on_key(repo, pipelines, key);
                            }
                            None => {
                                listening = false;
                                std::thread::sleep(slice);
                            }
                        }
                    } else if !listening {
                        std::thread::sleep(slice);
                    }
                }
            }
            None => {
                println!(
                    "  next pass in {}",
                    crate::config::format_duration(interval)
                );
                // Cut into the same slices the board's wait takes, so a stop
                // asked for during the wait is noticed then rather than a whole
                // interval later.
                let until = std::time::Instant::now() + interval;
                while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
                    if crate::platform::stop::asked() {
                        break;
                    }
                    std::thread::sleep(crate::status::POLL.min(left));
                }
            }
        }
    }
}

/// The first step, if any, whose `run:` calls `spoolway stack` — the one
/// command in this project that builds a commit (`commit-tree`, for its
/// squash) whether or not `dispatch.auto_commit` is on. Named rather than a
/// bare bool: the git-identity refusal below points at the actual step id
/// instead of assuming every pipeline still calls it `handover`, and
/// [`crate::commands::doctor`]'s own `reaches_forge` reuses this same
/// answer for the other thing that step needs, `gh`.
pub(crate) fn stack_step(pipelines: &Pipelines) -> Option<&str> {
    pipelines
        .pipelines
        .values()
        .flat_map(|pipeline| &pipeline.steps)
        .find(|step| {
            step.run
                .as_deref()
                .is_some_and(|run| run.contains("spoolway stack"))
        })
        .map(|step| step.id.as_str())
}

/// Why this project would ever run `git commit` on its own — `None` for a
/// project that never does, which is the one case the git-identity check
/// below must leave alone: requiring an identity from a project that commits
/// nothing would refuse a start over a setting nobody needs.
fn commit_reason(pipelines: &Pipelines, config: &Config) -> Option<String> {
    if let Some(step) = stack_step(pipelines) {
        return Some(format!("`{step}` runs `spoolway stack`"));
    }
    config
        .dispatch
        .auto_commit
        .then(|| "`dispatch.auto_commit` is on".to_string())
}

/// One of the three things [`check_git_identity`], [`check_index_lock`] and
/// [`check_backend_checkout`] refuse on.
///
/// `reason` is the short fact — `doctor`'s own row, `Report::check`'s `why`
/// verbatim, and exactly what the task's own mockup draws next to `FAIL`.
/// `fix` is the rest: why it matters and the command a person pastes to
/// clear it, added only to `dispatch`'s own refusal below, which is the one
/// place "what do I type" actually belongs.
///
/// A distinct error type, downcast out of the three checks' `?` the same way
/// [`RestartsRefused`] already is downcast in `crate::main` — plain `?`
/// alone would flatten this to `reason`, which is right for `doctor` and
/// wrong for `dispatch`.
#[derive(Debug)]
struct Refusal {
    reason: String,
    fix: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for Refusal {}

/// [`Refusal::fix`] appended to [`Refusal::reason`], for `dispatch`'s own
/// refusal — the fuller message the task's mockup draws under
/// `spoolway dispatch`. A `doctor` row never calls this: `Report::check`
/// reads a [`Refusal`]'s plain `{err:#}`, which is `reason` alone.
fn refuse(check: Result<Option<String>>) -> Result<Option<String>> {
    check.map_err(|err| match err.downcast::<Refusal>() {
        Ok(refusal) => anyhow!("{} — {}", refusal.reason, refusal.fix),
        Err(err) => err,
    })
}

/// Refuse a start (or fail a `doctor` row) when this project is about to
/// commit and git has nowhere to put a name on the commit.
///
/// `dispatch.auto_commit` sweeps a lane's own leftovers into a `wip(...)`
/// commit on every step, and `spoolway stack`'s squash builds one with
/// `commit-tree` regardless of that setting — both fail outright with no git
/// identity configured, and both used to fail silently mid-run, well past
/// the point a person could fix it without losing anything.
///
/// `config` is handed in rather than read off `repo.config`, the same as
/// [`commit_reason`] already takes it: `doctor` answers every other finding
/// from the checkout's own loaded copy (see its module doc), and this row
/// is no exception — a task branch that turns `auto_commit` on in its own
/// checkout must see this row change, not `repo.root`'s copy silently
/// standing in for it.
pub(crate) fn check_git_identity(
    repo: &Repo,
    pipelines: &Pipelines,
    config: &Config,
) -> Result<Option<String>> {
    let Some(reason) = commit_reason(pipelines, config) else {
        return Ok(None);
    };

    let missing: Vec<&str> = ["user.name", "user.email"]
        .into_iter()
        .filter(|key| {
            repo.git(&["config", key])
                .unwrap_or_default()
                .trim()
                .is_empty()
        })
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }

    let (verb, pronoun) = match missing.len() {
        1 => ("is", "it"),
        _ => ("are", "them"),
    };
    // A placeholder per key: `user.email` reads fine as a bare address, but
    // handing the same address back for `user.name` would have somebody
    // paste their commit author name as an email — the defect this once was.
    let placeholder = |key: &str| match key {
        "user.name" => "\"Your Name\"",
        _ => "you@example.com",
    };
    let commands = missing
        .iter()
        .map(|key| format!("  git config --global {key} {}", placeholder(key)))
        .collect::<Vec<_>>()
        .join("\n");
    Err(anyhow::Error::new(Refusal {
        reason: format!("git {} {verb} not set, and {reason}", missing.join(" and ")),
        fix: format!("the first commit would fail. Set {pronoun} with:\n\n{commands}"),
    }))
}

/// Refuse a start (or fail a `doctor` row) on a leftover `.git/index.lock` —
/// the one file that fails every git write while `git status` itself
/// answers normally, so nothing else here would ever notice it.
///
/// The real git directory, asked of git rather than assumed to be `.git`:
/// under a linked worktree it is `.git/worktrees/<name>` instead, and this
/// runs against `repo.root`, the checkout the dispatcher itself commits in.
pub(crate) fn check_index_lock(repo: &Repo) -> Result<Option<String>> {
    let git_dir = repo.git(&["rev-parse", "--git-dir"])?;
    let lock = repo.root.join(git_dir.trim()).join("index.lock");
    if lock.exists() {
        return Err(anyhow::Error::new(Refusal {
            reason: format!("{} exists", lock.display()),
            fix: format!(
                "every git write here fails until it is gone. If nothing is actually \
                 mid-commit, remove it:\n\n  rm {}",
                lock.display()
            ),
        }));
    }
    Ok(None)
}

/// Refuse a start (or fail a `doctor` row) on a backend that cannot actually
/// open this checkout — caught here rather than left for the first lane's
/// own workspace to fail on, since nothing about it is likely to change
/// between one dispatch pass and the next.
///
/// A linked worktree is not this: `Herdr::new` resolves its own
/// `project_root` through [`crate::repo::main_checkout`] before it ever
/// opens anything, in `src/mux.rs`, and hands *that* to `worktree open` as
/// `--cwd` — so herdr is never actually given a linked worktree to refuse.
/// What it genuinely cannot open is a checkout `main_checkout` cannot
/// resolve at all, the same call `Herdr::new` itself makes — a bare
/// repository, or a `.git` too unusual for it to place. Resolved here the
/// same way, so this only refuses what herdr would too.
pub(crate) fn check_backend_checkout(
    repo: &Repo,
    mux: &dyn crate::mux::Mux,
) -> Result<Option<String>> {
    if !mux.is_available() {
        bail!("{}", mux.unavailable());
    }
    if mux.name() == "herdr" && crate::repo::main_checkout(&repo.root).is_none() {
        return Err(anyhow::Error::new(Refusal {
            reason: format!(
                "{} has no main checkout herdr can resolve a workspace onto",
                repo.root.display()
            ),
            fix: "a bare repository, say. Switch backends:\n\n  spoolway config set \
                  dispatch.backend tmux"
                .to_string(),
        }));
    }
    Ok(Some(format!("{} · main checkout", mux.name())))
}

/// Draw the read-only board over a run someone else's process is driving.
///
/// The queue re-reads and the multiplexer call the board already makes are
/// the whole of what this needs — see [`crate::status::Board::watching`] —
/// so nothing here takes the lock, starts a lane, writes a task file, or
/// moves a pane. `ctrl-c` ends this loop and this loop alone; the run it is
/// watching is someone else's to stop.
fn watch(repo: &Repo, pipelines: &Pipelines, args: &DispatchArgs) -> Result<()> {
    if args.plain {
        // One read, one print, no loop: a script wants the table as it
        // stands, not a process that sits there polling on its behalf.
        let holder = crate::lock::Lock::holder(&repo.lock_file())?;
        match holder {
            Some(pid) => println!("watching dispatcher (pid {pid})\n"),
            None => println!("no dispatcher is running\n"),
        }
        let rows = crate::status::rows(repo, pipelines)?;
        print!("{}", crate::status::plain_table(&rows));
        return Ok(());
    }

    crate::platform::stop::catch_interrupt();
    let mut board = crate::status::Board::watching();
    let mut out = std::io::stdout();
    // `Phase::Waiting` throughout: a watcher never passes and never stops a
    // run, so neither of the other two phases means anything here — the
    // header this board draws comes from the lock, not from a phase this
    // process is in.
    while !crate::platform::stop::asked() {
        let _ = board.draw(repo, pipelines, crate::status::Phase::Waiting, &mut out);
        std::thread::sleep(crate::status::POLL);
    }
    Ok(())
}

/// Settle the books on what the run was holding, and draw the last frame.
///
/// Both ways a dispatcher ends come through here — the queue emptying, and a
/// person asking it to stop — because they leave the same things behind and
/// differ only in how much of it there is. Nothing is torn down either way:
/// see [`crate::dispatch::Dispatcher::sweep_on_stop`].
///
/// A settle that fails is reported and not raised: the run is over either
/// way, and an unbanked lane is something to catch up by hand, not a reason
/// to exit non-zero.
fn stop(
    repo: &Repo,
    pipelines: &Pipelines,
    mux: &dyn crate::mux::Mux,
    board: Option<&mut crate::status::Board>,
    out: &mut std::io::Stdout,
    args: &DispatchArgs,
) -> Result<()> {
    if !args.dry_run {
        let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux, args.dry_run);
        let mut report = crate::dispatch::Report::default();
        match dispatcher.sweep_on_stop(&mut report) {
            Ok(()) => {
                for action in &report.actions {
                    println!("  {action}");
                }
            }
            Err(err) => println!("  ! could not settle this run's lanes: {err:#}"),
        }
    }

    // The last frame stays where it is and the reason the run ended is printed
    // under it: the board is on the main screen, so what it drew is still there
    // to read when the process exits.
    if let Some(board) = board {
        let _ = board.draw(repo, pipelines, crate::status::Phase::Stopping, out);
    }
    Ok(())
}

/// Print the "a job keeps this run resident" lines under the plain run. The
/// board shows the same facts its own way — see [`crate::status`].
fn print_staying_up(jobs: &crate::jobs::StayingUp) {
    for line in crate::jobs::staying_up_lines(jobs) {
        println!("  {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// A dispatcher already running for this repo must send `spoolway
    /// dispatch` down [`watch`], never through [`crate::lock::Lock::acquire`]
    /// — the watcher reads the run, it never joins it.
    ///
    /// Proved indirectly rather than by mocking `Lock::acquire`: this process
    /// takes the lock itself first, the same way a real dispatcher would, and
    /// then calls `dispatch` again against the same repo. `Lock::acquire`
    /// refuses a second holder outright — see its own doc comment — so a
    /// second acquire attempt here would surface as this call returning an
    /// error naming "already running", not as a silent takeover. `--plain`
    /// keeps this one call and one exit, with no board loop to interrupt.
    #[test]
    fn dispatch_watches_rather_than_taking_the_lock() {
        let repo = fixture("watch-never-locks");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false).unwrap();

        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        let result = dispatch(&repo, &Pipelines::builtin(), &args);

        assert!(
            result.is_ok(),
            "a second `Lock::acquire` on the path `dispatch` took would have failed instead of \
             returning cleanly: {result:?}"
        );
    }

    /// Four starts in a row that could not run at all — here, four starts
    /// against a repo whose lock another dispatcher already holds — have to
    /// get the fifth refused, naming the count, the last reason and
    /// `spoolway dispatch --force`. Nothing today counts a start that could
    /// not run, so this run of five plain, back-to-back calls all return
    /// `Ok`: the fifth watches the held lock exactly like the first four,
    /// instead of being turned away as a restart storm.
    #[test]
    fn four_starts_that_could_not_run_get_the_fifth_refused() {
        let repo = fixture("restart-storm");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false).unwrap();

        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };

        for attempt in 1..=4 {
            let result = dispatch(&repo, &Pipelines::builtin(), &args);
            assert!(
                result.is_ok(),
                "attempt {attempt} of 4 should still just watch the held lock, not refuse yet: \
                 {result:?}"
            );
        }

        let fifth = dispatch(&repo, &Pipelines::builtin(), &args);
        assert!(
            fifth.is_err(),
            "a fifth start, after four in a row that could not run at all, should be refused \
             naming the count and `spoolway dispatch --force` — instead it watched the held \
             lock like any other start: {fifth:?}"
        );
        let message = format!("{:#}", fifth.unwrap_err());
        assert!(message.contains("4 starts in a row"), "{message}");
        assert!(message.contains("spoolway dispatch --force"), "{message}");
    }

    /// An empty queue is an ordinary ending, exit code 3 — distinct from a
    /// lock already held so a caller restarting this in a loop can tell the
    /// two apart.
    #[test]
    fn an_empty_queue_exits_three() {
        let repo = fixture("empty-queue-exit");
        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
            EXIT_EMPTY_QUEUE
        );
    }

    /// A lock already held exits 4, whether or not it is the first start to
    /// find it that way.
    #[test]
    fn a_lock_already_held_exits_four() {
        let repo = fixture("lock-held-exit");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false).unwrap();
        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
            EXIT_ALREADY_RUNNING
        );
    }

    /// An empty queue is never counted towards the restart guard: a repo
    /// with nothing to do is not a storm, so restarting into one forever —
    /// far more than four times — must never be refused.
    #[test]
    fn an_empty_queue_is_never_counted_towards_a_restart_storm() {
        let repo = fixture("empty-queue-not-counted");
        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        for attempt in 1..=8 {
            let result = dispatch(&repo, &Pipelines::builtin(), &args);
            assert!(
                result.is_ok(),
                "attempt {attempt} against an empty queue should never be refused: {result:?}"
            );
        }
    }

    /// `--force` clears an already-standing count, the same as a start that
    /// actually runs — the fifth start, refused above, goes through once
    /// asked to force its way past the guard.
    #[test]
    fn force_clears_the_restart_counter() {
        let repo = fixture("force-clears-storm");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false).unwrap();

        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        for _ in 1..=4 {
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap();
        }
        assert!(dispatch(&repo, &Pipelines::builtin(), &args).is_err());

        let forced = DispatchArgs {
            plain: true,
            force: true,
            ..Default::default()
        };
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &forced).unwrap(),
            EXIT_ALREADY_RUNNING,
            "a forced start should go through even with a storm standing, and clear it"
        );

        // The forced start itself still could not run — the lock is held
        // throughout this test — so it counts as one refusal of its own
        // fresh count, cleared and restarted at zero by `--force`. Three
        // more take it to 4 again; the fourth of those is the one refused.
        for attempt in 1..=3 {
            let result = dispatch(&repo, &Pipelines::builtin(), &args);
            assert!(
                result.is_ok(),
                "attempt {attempt} of 3 after --force cleared the count should not be refused: \
                 {result:?}"
            );
        }
        assert!(dispatch(&repo, &Pipelines::builtin(), &args).is_err());
    }

    /// A pipeline whose only step names no `run:` at all — standing in for a
    /// project whose pipeline never reaches `spoolway stack`.
    fn single_step_pipelines() -> Pipelines {
        let pipeline: Pipeline =
            serde_norway::from_str("steps:\n  - id: a\n    end: true\n").unwrap();
        let mut pipelines = std::collections::BTreeMap::new();
        pipelines.insert("default".to_string(), pipeline);
        Pipelines {
            default: "default".to_string(),
            pipelines,
        }
    }

    /// Blank both `user.name` and `user.email` locally, overriding whatever
    /// the machine running this test has configured globally — the same way
    /// `git config user.name ""` reads to `check_git_identity` as unset:
    /// `git config user.name` still exits 0 and prints nothing, and
    /// `.trim().is_empty()` is exactly what that check reads. Blanking
    /// locally rather than touching `$HOME` keeps this off the shared,
    /// process-global state a parallel test run cannot race on.
    fn blank_git_identity(repo: &Repo) {
        repo.git(&["config", "user.name", ""]).unwrap();
        repo.git(&["config", "user.email", ""]).unwrap();
    }

    /// `stack_step` finds the shipped `handover` step, by name, in both
    /// built-in pipelines.
    #[test]
    fn stack_step_finds_the_shipped_handover_step() {
        assert_eq!(stack_step(&Pipelines::builtin()), Some("handover"));
    }

    /// A pipeline with no `spoolway stack` step and `auto_commit` off never
    /// asks git for anything: this project commits nothing, so an absent
    /// identity is nobody's problem — the non-goal this check exists to
    /// leave alone.
    #[test]
    fn git_identity_is_not_checked_when_nothing_commits() {
        let mut repo = fixture("git-identity-nothing-commits");
        blank_git_identity(&repo);
        repo.config.dispatch.auto_commit = false;
        assert!(
            check_git_identity(&repo, &single_step_pipelines(), &repo.config)
                .unwrap()
                .is_none()
        );
    }

    /// A blank `user.email`, with the shipped `handover` step reaching
    /// `spoolway stack`, is refused — naming the missing key and the reason
    /// in the short form `doctor`'s own row reads (`{err:#}` on the check
    /// itself, never wrapped through [`refuse`]), and a command that sets
    /// it once wrapped for `dispatch`'s own refusal.
    #[test]
    fn git_identity_is_refused_when_the_shipped_pipeline_reaches_stack() {
        let repo = fixture("git-identity-missing");
        blank_git_identity(&repo);

        let bare = format!(
            "{:#}",
            check_git_identity(&repo, &Pipelines::builtin(), &repo.config).unwrap_err()
        );
        assert!(bare.contains("user.name"), "{bare}");
        assert!(bare.contains("user.email"), "{bare}");
        assert!(bare.contains("`handover` runs `spoolway stack`"), "{bare}");
        assert!(
            !bare.contains("git config"),
            "a doctor row must carry the short reason alone, not the fix: {bare}"
        );

        let refused = format!(
            "{:#}",
            refuse(check_git_identity(
                &repo,
                &Pipelines::builtin(),
                &repo.config
            ))
            .unwrap_err()
        );
        assert!(refused.starts_with(&bare), "{refused}");
        assert!(
            refused.contains("git config --global user.name \"Your Name\""),
            "a name placeholder must read as a name, not an email address: {refused}"
        );
        assert!(
            refused.contains("git config --global user.email you@example.com"),
            "{refused}"
        );
    }

    /// `dispatch.auto_commit` alone — no pipeline step reaching `spoolway
    /// stack` at all — is reason enough: `auto_commit` commits on its own.
    #[test]
    fn git_identity_is_refused_for_auto_commit_alone() {
        let repo = fixture("git-identity-auto-commit-alone");
        blank_git_identity(&repo);
        let err = check_git_identity(&repo, &single_step_pipelines(), &repo.config).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("`dispatch.auto_commit` is on"),
            "{message}"
        );
    }

    /// The identity `git_init` gave the fixture is real, so a project that
    /// does commit and does have an identity passes clean.
    #[test]
    fn git_identity_passes_when_it_is_set() {
        let repo = fixture("git-identity-set");
        assert!(
            check_git_identity(&repo, &Pipelines::builtin(), &repo.config)
                .unwrap()
                .is_none()
        );
    }

    /// `doctor` reads a checkout's own loaded config, never `repo.config`
    /// (`repo.root`'s) — see the module doc on `src/commands/doctor.rs`.
    /// `check_git_identity` takes `Config` as a parameter for exactly that:
    /// a repo whose own `repo.config` never turned `auto_commit` on still
    /// gets the row once a *different* config, standing in for the
    /// checkout's, does.
    #[test]
    fn git_identity_reads_the_config_it_is_handed_not_repos_own() {
        let mut repo = fixture("git-identity-checkout-config");
        blank_git_identity(&repo);
        repo.config.dispatch.auto_commit = false;
        let mut checkout_config = repo.config.clone();
        checkout_config.dispatch.auto_commit = true;

        assert!(
            check_git_identity(&repo, &single_step_pipelines(), &repo.config)
                .unwrap()
                .is_none(),
            "repo.config itself never turned auto_commit on"
        );
        assert!(
            check_git_identity(&repo, &single_step_pipelines(), &checkout_config).is_err(),
            "the config actually handed in did, and must be the one read"
        );
    }

    /// A leftover `.git/index.lock` is refused — the short reason names the
    /// path, and `refuse`'s longer form adds the command to clear it; its
    /// absence is a pass.
    #[test]
    fn index_lock_is_refused_when_present_and_clear_otherwise() {
        let repo = fixture("index-lock");
        assert!(check_index_lock(&repo).unwrap().is_none());

        let lock = repo.root.join(".git/index.lock");
        std::fs::write(&lock, "").unwrap();

        let bare = format!("{:#}", check_index_lock(&repo).unwrap_err());
        assert!(bare.contains("index.lock"), "{bare}");
        assert!(
            !bare.contains("rm "),
            "a doctor row must carry the short reason alone, not the fix: {bare}"
        );

        let refused = format!("{:#}", refuse(check_index_lock(&repo)).unwrap_err());
        assert!(
            refused.contains(&format!("rm {}", lock.display())),
            "{refused}"
        );

        std::fs::remove_file(&lock).unwrap();
        assert!(check_index_lock(&repo).unwrap().is_none());
    }

    /// A bare repository stands in for a checkout `main_checkout` cannot
    /// place — the one case herdr genuinely cannot open a workspace on
    /// (`Herdr::new` makes the same call before it ever opens anything).
    fn bare_repo(name: &str) -> Repo {
        let dir = crate::scratch::root(&format!("bare-checkout-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        crate::repo::run(&dir, "git", &["init", "-q", "--bare"]).unwrap();
        Repo {
            root: dir.clone(),
            checkout: dir.clone(),
            config: Config::default(),
            home: dir.join(".home"),
        }
    }

    /// A `Mux` standing in for herdr (or anything else), answering only
    /// `name`/`is_available` for real — the only two [`check_backend_checkout`]
    /// ever reads — and refusing every other call outright, so a test that
    /// somehow reached one fails loudly rather than doing something real.
    struct StubMux {
        name: &'static str,
        available: bool,
    }

    impl Mux for StubMux {
        fn name(&self) -> &'static str {
            self.name
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn unavailable(&self) -> String {
            "the stub backend is never available".into()
        }
        fn resident_while_waiting(&self) -> bool {
            unimplemented!()
        }
        fn list_lanes(&self) -> Result<Vec<crate::mux::Lane>> {
            unimplemented!()
        }
        fn create_workspace(
            &self,
            _cwd: &std::path::Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
            unimplemented!()
        }
        fn remove_workspace(&self, _workspace_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_workspace(&self, _workspace_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_tab(&self, _tab_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn create_pane(
            &self,
            _cwd: &std::path::Path,
            _label: &str,
        ) -> Result<crate::mux::Workspace> {
            unimplemented!()
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &std::path::Path) -> Result<String> {
            unimplemented!()
        }
        fn close_pane(&self, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn start_lane(&self, _spec: &crate::mux::LaneSpec<'_>) -> Result<()> {
            unimplemented!()
        }
        fn prompt(&self, _name: &str, _text: &str) -> Result<()> {
            unimplemented!()
        }
        fn read(&self, _name: &str, _lines: usize) -> Result<String> {
            unimplemented!()
        }
        fn interrupt_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn stop_lane(&self, _name: &str, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn focus_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_workspace(&self, _workspace_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
    }

    /// A backend that is not herdr is never refused over a bare repository:
    /// only herdr binds a workspace to a resolved main checkout at all.
    #[test]
    fn backend_checkout_is_unconcerned_with_a_non_herdr_backend() {
        let repo = bare_repo("non-herdr");
        let mux = StubMux {
            name: "tmux",
            available: true,
        };
        assert!(check_backend_checkout(&repo, &mux).unwrap().is_some());
    }

    /// herdr against an ordinary checkout — the fixture's own — resolves a
    /// main checkout fine and passes.
    #[test]
    fn backend_checkout_passes_herdr_on_an_ordinary_checkout() {
        let repo = fixture("herdr-ordinary-checkout");
        let mux = StubMux {
            name: "herdr",
            available: true,
        };
        assert!(check_backend_checkout(&repo, &mux).unwrap().is_some());
    }

    /// herdr against a bare repository — no main checkout `main_checkout`
    /// can place, so `Herdr::new` itself would fall back to a `--cwd` it
    /// cannot open either — is refused, naming the path and a way out.
    #[test]
    fn backend_checkout_refuses_herdr_on_a_bare_repository() {
        let repo = bare_repo("herdr-bare");
        let mux = StubMux {
            name: "herdr",
            available: true,
        };
        let bare = format!("{:#}", check_backend_checkout(&repo, &mux).unwrap_err());
        assert!(bare.contains(&repo.root.display().to_string()), "{bare}");
        assert!(
            !bare.contains("dispatch.backend"),
            "a doctor row must carry the short reason alone, not the fix: {bare}"
        );

        let refused = format!(
            "{:#}",
            refuse(check_backend_checkout(&repo, &mux)).unwrap_err()
        );
        assert!(refused.contains("dispatch.backend"), "{refused}");
    }
}
