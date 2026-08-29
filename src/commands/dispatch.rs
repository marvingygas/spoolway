//! `spoolway dispatch`: the run loop around [`crate::dispatch`], and stopping one.

use anyhow::anyhow;

use super::*;
use crate::dispatch::{RESTART_MAX, RESTART_WINDOW};
use crate::screen::PollableRead;

/// An empty queue is an ordinary ending: nothing was there to dispatch, and
/// running again once something is queued is exactly what a person or a
/// script should do next.
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

    // Nothing queued is nothing to dispatch. Without this the loop would sit
    // there printing "nothing to do" until somebody noticed, and a dispatcher
    // started in the wrong project looks exactly like one with no work yet.
    //
    // Not counted: a repo with nothing to do is not a storm, and restarting
    // into an empty queue forever is a caller's own choice to make, not
    // something this guard has any business refusing.
    if repo.tasks()?.is_empty() {
        println!("nothing is queued, so there is nothing to dispatch.");
        println!("  spoolway queue add --from <path>");
        return Ok(EXIT_EMPTY_QUEUE);
    }

    // A start that gets this far can actually run. Whatever the guard above
    // was counting, it was counting starts that could not — this is not one
    // of them, so the slate is clean again.
    crate::lock::Restarts::clear(&repo.restarts_file())?;

    let mux = crate::mux::backend(repo);
    if !mux.is_available() {
        bail!("{}", mux.unavailable());
    }

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
    // From here the run holds live lanes and worktrees, so it has something to
    // end and give back on the way out. Caught rather than left to kill the
    // process where it stands — see `dispatch.tear_lanes_on_stop`.
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
            // A queue that cannot be read is a reason to try again next pass,
            // not to decide the work is finished.
            _ => {}
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

/// Give back what the run was holding, and draw the last frame.
///
/// Both ways a dispatcher ends come through here — the queue emptying, and a
/// person asking it to stop — because they leave the same things behind and
/// differ only in how much of it there is. Whether anything is actually swept
/// is `dispatch.tear_lanes_on_stop`'s to say.
///
/// A sweep that fails is reported and not raised: the run is over either way,
/// and a teardown error is a worktree to remove by hand, not a reason to exit
/// non-zero.
fn stop(
    repo: &Repo,
    pipelines: &Pipelines,
    mux: &dyn crate::mux::Mux,
    board: Option<&mut crate::status::Board>,
    out: &mut std::io::Stdout,
    args: &DispatchArgs,
) -> Result<()> {
    if repo.config.dispatch.tear_lanes_on_stop && !args.dry_run {
        let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux, args.dry_run);
        let mut report = crate::dispatch::Report::default();
        match dispatcher.sweep_on_stop(&mut report) {
            Ok(()) => {
                for action in &report.actions {
                    println!("  {action}");
                }
            }
            Err(err) => println!("  ! could not give back this run's worktrees: {err:#}"),
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
}
