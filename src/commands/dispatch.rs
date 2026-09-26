//! `spoolway dispatch`: the run loop around [`crate::dispatch`], and stopping one.

use anyhow::anyhow;

use super::*;
use crate::dispatch::{RESTART_MAX, RESTART_WINDOW, skip_wait};
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

/// Run the pipeline: one pass, or a loop on the dispatcher's fixed poll rate.
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
        already_running(repo, pipelines, pid, args, &mut std::io::stdout())?;
        return Ok(EXIT_ALREADY_RUNNING);
    }

    // Board::new() is what takes the terminal — hidden cursor, raw enough
    // mode — and it is built here, ahead of every check below, rather than
    // once the loop starts: nothing used to be drawn until the loop's first
    // frame, so every check before it was dead screen time. Building it
    // early is what gives the checklist below somewhere to write, and what
    // makes its guard the one the three gates further down reuse instead of
    // nesting a second one of their own — see `own_term`, below.
    //
    // Not for `--plain`, which keeps its own one-line-per-pass log and gains
    // none of this: no board, no terminal taken, no checklist printed.
    let mut board = match args.plain {
        true => None,
        false => Some(crate::status::Board::new()),
    };
    let mut out = std::io::stdout();
    let checklist = board.is_some();
    if checklist {
        print_checklist_header(&mut out)?;
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
    let live_tasks = checklist_row(
        &mut out,
        checklist,
        "queue read",
        "reading the queue",
        || repo.tasks(),
    )?;
    checklist_done(
        &mut out,
        checklist,
        "queue read",
        &plural(live_tasks.len(), "task"),
    )?;
    if live_tasks.is_empty() {
        if crate::jobs::enabled_count(repo) == 0 {
            println!("nothing is queued, so there is nothing to dispatch.");
            println!("  spoolway queue add --from <path>");
            return Ok(EXIT_EMPTY_QUEUE);
        }
        print_staying_up(&crate::jobs::staying_up(repo));
        idle_announced = true;
    }

    // Every live task's own routing source, checked whole before anything
    // starts: `pipeline:` is the only place a task's pipeline comes from any
    // more — there is no project default to fall back to — and a task that
    // cannot resolve one is broken whether or not a lane ever reaches it.
    // Refused here, ahead of the lock, for the same reason as the three
    // checks below: found and fixed by a person reading this line, not
    // discovered mid-run and left for someone to stop by hand.
    checklist_row(
        &mut out,
        checklist,
        "task routes",
        "checking routes",
        || check_task_routes(pipelines, &live_tasks),
    )?;
    checklist_done(
        &mut out,
        checklist,
        "task routes",
        &task_route_names(&live_tasks),
    )?;

    // A start that gets this far can actually run. Whatever the guard above
    // was counting, it was counting starts that could not — this is not one
    // of them, so the slate is clean again.
    crate::lock::Restarts::clear(&repo.restarts_file())?;

    let mux = crate::mux::backend(repo)?;
    checklist_row(
        &mut out,
        checklist,
        "backend available",
        &format!("starting {}", mux.name()),
        || match mux.is_available() {
            true => Ok(()),
            false => bail!("{}", mux.unavailable()),
        },
    )?;
    checklist_done(&mut out, checklist, "backend available", mux.name())?;

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
    checklist_row(
        &mut out,
        checklist,
        "git identity",
        "checking git identity",
        || refuse(check_git_identity(repo, pipelines, &repo.config)).context("refusing to start"),
    )?;
    checklist_done(&mut out, checklist, "git identity", "")?;
    checklist_row(
        &mut out,
        checklist,
        "index lock",
        "checking the index lock",
        || refuse(check_index_lock(repo)).context("refusing to start"),
    )?;
    checklist_done(&mut out, checklist, "index lock", "")?;
    checklist_row(
        &mut out,
        checklist,
        "backend checkout",
        "checking the checkout",
        || refuse(check_backend_checkout(repo, mux.as_ref())).context("refusing to start"),
    )?;
    checklist_done(&mut out, checklist, "backend checkout", "")?;

    // There is one way to start a run and it is visible: refused here, in the
    // same early group as the three checks above, so a run begun in a
    // backgrounded shell or a `backend = headless` config edit outside the
    // harness is stopped before it takes the lock or writes anywhere, not
    // found and killed by hand once it is already spending. No exemption —
    // not `--plain`, not an environment override, not a per-platform
    // carve-out.
    check_dispatcher_visible(mux.as_ref())?;

    // The last three things a person sees before anything is spawned or
    // written: the overview, naming every task the run is about to touch;
    // the overrides gate, since a layer changes what runs without `git
    // status` ever hinting that it is on; and doctor's own cheap findings,
    // read as a warning rather than discovered mid-run. All three run here,
    // before the lock is taken and the mode is written into it, so `esc` off
    // any of them can back out having done nothing at all. `args.confirmed`
    // skips all three: the queue screen's own `enter` already walked a
    // person through this same trio, reusing its own `TermGuard` rather than
    // nesting a second one — see `commands::queue::confirm_start`, which
    // calls `warnings_gate_with` itself rather than reading this one, since
    // it never runs this block at all.
    //
    // `own_term` is false whenever a board is up: its own guard, taken above
    // when the board was built, already has the cursor hidden and the tty
    // deaf, and a gate constructing a second one of its own here would be a
    // nested `TermGuard` — one whose `Drop`, firing the moment a gate
    // returns, shows the cursor and restores cooked mode while the board's
    // own guard is still held underneath it (`gate.rs` takes exactly this
    // shape for the one blocking read it owns, and drops it deliberately —
    // right there because nothing else is still holding the terminal when it
    // does). `--plain` has no board to reuse, so its own gates still take
    // their own guard, exactly as before this task.
    let own_term = board.is_none();
    if !args.confirmed {
        if !overview_gate(repo, own_term)? {
            return Ok(0);
        }
        if !overrides_gate(repo, own_term)? {
            return Ok(0);
        }
        if !warnings_gate(repo, pipelines, own_term)? {
            return Ok(0);
        }
    }

    // Taken for the whole run. Two dispatchers would both see the same task at
    // the same step and both spawn a lane into its worktree.
    //
    // The pane is asked for here rather than trusted from the environment —
    // same reasoning as `Mux::in_own_pane`'s own read, which
    // `check_dispatcher_visible` just passed: `None` here only degrades to
    // the older three-line lock shape, never refuses the start.
    let pane_id = mux.own_pane_id();
    let _lock = crate::lock::Lock::acquire(&repo.lock_file(), unattended, pane_id.as_deref())?;

    // Note this project once per run, so `spoolway eval --all` can find
    // its ledger later. A project that is dispatched in is a project that spends.
    crate::usage::registry::register(&repo.root);

    // Trimmed once, here, rather than on every append below — see
    // `crate::problem_log::open`. A run that never hits a problem never
    // touches this file at all.
    crate::problem_log::open(repo);

    let interval = crate::dispatch::PROBE_INTERVAL;

    // The dispatcher's own pane stays wherever it was started, under both
    // `grouped` and `split` — herdr has nothing to move it into. This still
    // has to find or open the run's shared workspace, though, under
    // `grouped`: a task's lane joins that workspace's tab, and it must exist
    // before the first one starts.
    //
    // A failure here is held for `workspace_open_notice`, just below, rather
    // than printed on the spot — this is the one notice `warnings_gate`,
    // above, could not carry: this is only attempted once the lock is held,
    // past the point `esc` could still mean "nothing happened yet". Its own
    // pending row is printed and cleared here rather than through
    // `checklist_row`, because its error must not abort the run the way
    // every other row's does — it is reported through `workspace_open_notice`
    // and stepped over instead.
    checklist_pending(&mut out, checklist, "workspace", "opening the workspace")?;
    let mut workspace_open_error = None;
    let mut workspace_id = None;
    match mux.dispatch_workspace(&repo.root, true) {
        Ok(id) => workspace_id = id,
        Err(err) => {
            workspace_open_error = Some(format!("could not open this run's own workspace: {err:#}"))
        }
    }
    checklist_clear(&mut out, checklist)?;

    // The one notice `warnings_gate` ran too early to carry — see just
    // above. Held on screen the same way, but with only `[enter]` to
    // dismiss it: by now the lock is held, so there is no earlier screen
    // left for `esc` to mean "back to" — see `workspace_open_notice`'s own
    // doc.
    match &workspace_open_error {
        Some(err) => workspace_open_notice(err, own_term)?,
        None => checklist_done(
            &mut out,
            checklist,
            "workspace",
            workspace_id.as_deref().unwrap_or("—"),
        )?,
    }

    // The run watches itself. A resident dispatcher spends almost all of its
    // time waiting for the next pass, and drawing the board through that wait
    // costs the run nothing — where a scrolling log says only what the last
    // pass did, the board says what the whole queue is doing right now.
    //
    // Not for `--plain`, which is a person who would rather have the log — a
    // pipe, a CI job, a terminal that mangles the redraw.
    // From here the run holds live lanes, so an interrupted one's spend and
    // launch counter still have to be settled on the way out. Caught rather
    // than left to kill the process where it stands — see
    // `crate::dispatch::Dispatcher::sweep_on_stop`.
    crate::platform::stop::catch_interrupt();

    // Consecutive passes that skipped the wait because the one before it
    // moved a task — see the check ahead of the wait, below. Reset the
    // moment a pass finds nothing to move, so the count only ever measures
    // one unbroken streak, never the run's total.
    let mut consecutive_working: u32 = 0;

    // Wakes the wait below the moment a lane's own `spoolway report` or a
    // finished background command lands, instead of it being found up to
    // `interval` later — see `crate::screen::DirWatch`. `None` on a target
    // with nothing to watch with, or if opening the watch failed for some
    // other reason; either way the wait below falls back to the plain
    // interval alone, exactly as it behaved before this task.
    let watch = crate::screen::open_dir_watch(&repo.queue_dir(), &repo.commands_dir());

    loop {
        let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux.as_ref());

        // Drawn before the pass rather than only after it, so a pass that takes
        // a while is a board saying "pass running" instead of a blank terminal.
        //
        // When it was last drawn, so the callback below can keep the same
        // once-a-second cadence the wait keeps — see the tick's own comment.
        let mut drawn_at = std::time::Instant::now();
        if let Some(board) = board.as_mut() {
            let _ = board.draw(repo, pipelines, crate::status::Phase::Passing, &mut out);
        }

        // Stdin, and whether it is still worth reading. Declared once per
        // pass rather than separately for the callback below and the wait
        // loop further down, so a descriptor found gone (a closed pipe, no
        // controlling terminal) while a pass was still working is not asked
        // again a moment later by the wait that follows it — one direction
        // only, within this one iteration: the next iteration's own pass
        // starts the question over, the same as it always has.
        //
        // `listening` is cleared once stdin is found to have gone away, and
        // never asked again this iteration. `poll_ready` reports a closed
        // descriptor "ready" exactly as it does a real keystroke, since the
        // read that follows either way returns promptly; without this guard,
        // whichever of the callback or the wait loop reads next would keep
        // taking that as a key, get `None` back from `read_key` every time,
        // and spin down to nothing for the rest of the run (jobs review
        // finding 8). What counts as "gone away" is a run of empty reads
        // rather than one — see [`EMPTY_READS_BEFORE_DEAF`], which is also
        // why a single interrupted read no longer costs the keyboard.
        //
        // Starts false on a target with no raw mode to listen through
        // (`!cfg!(unix)`) or in `--plain` (`board.is_none()`, which never
        // reads a key at all): a cooked stdin answers `byte_pending` with
        // `false` at once (see `RawStdin`), and starting out listening
        // there would spin on that answer with no sleep in it.
        let mut stdin = crate::screen::RawStdin;
        let mut listening = cfg!(unix) && board.is_some();

        let mut spent_out = None;
        // Whether this pass moved a task and so has more ready to try at
        // once — see the wait below, and `dispatch::skip_wait`. A pass that
        // errored outright never sets this: a transient failure should cost
        // one wait, not be retried with no pause at all.
        let mut worked = false;

        // The callback a pass calls between its own units of work — see
        // `Dispatcher::pass`. Draining whatever is already on stdin
        // and applying it here is what keeps the board's own keys answering
        // at the same rate through a busy run as an idle one: a pass used
        // to hold the keyboard dead for its whole duration, and
        // `consecutive_working` below could run a hundred of those back to
        // back before the wait loop this used to live in alone was ever
        // reached again.
        //
        // Scoped to this block so its borrow of `board`/`out` ends the
        // moment the pass returns, freeing both for the rest of the loop
        // body below.
        let pass_result = {
            let mut tick = || {
                let Some(board) = board.as_mut() else { return };
                let keyed = drain_keys(repo, pipelines, board, &mut stdin, &mut listening);
                // A pass is not a pause in the run, and the board must not
                // read as one. Under a real multiplexer a pass runs for
                // whole seconds at a stretch, and the one frame drawn above
                // it would be the only thing on screen for all of them: the
                // lockup stuck on the phase it was drawn at, the header's
                // `up` and every row's TIME standing still, while the run
                // moves on underneath. So a frame is owed here on the same
                // once-a-second cadence the wait keeps, and one straight
                // away wherever a key has just changed what it would show —
                // a person who pressed something should not wait out the
                // rest of the pass to see it land.
                //
                // What that covers is exactly what `Dispatcher::pass` calls
                // this from, and no more — its own doc has the list. The
                // long stretch it reaches is a lane start's wait on the
                // herdr child it spawned, which hands `tick` back every
                // `VACATE_POLL`; the rest are the checkpoints between units
                // of work. Cutting the worktree is not among them:
                // `dispatch::ensure_workspace` takes no `tick` and runs
                // before the lane start that does, so the board still holds
                // its last frame for as long as that takes.
                //
                // Capped by the clock rather than drawn at every tick point:
                // a pass walking a long queue calls this between each task,
                // and a frame per task would be a redraw storm that also
                // asks the multiplexer for its lane list every time.
                if tick_owes_a_frame(keyed, drawn_at.elapsed()) {
                    let _ = board.draw(repo, pipelines, crate::status::Phase::Passing, &mut out);
                    drawn_at = std::time::Instant::now();
                }
            };
            dispatcher.pass(&mut tick)
        };
        match pass_result {
            Ok(report) => {
                worked = skip_wait(&report, consecutive_working);
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

        // Spent, and nothing left running to spend more. The queue keeps its
        // place: every task is where its last lane left it, and the next run
        // picks them up from exactly there.
        if let Some(note) = spent_out {
            stop(repo, pipelines, mux.as_ref(), board.as_mut(), &mut out)?;
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
            stop(repo, pipelines, mux.as_ref(), board.as_mut(), &mut out)?;
            println!("  stopped.");
            return Ok(0);
        }

        match repo.tasks() {
            Ok(tasks) if tasks.is_empty() => {
                if crate::jobs::enabled_count(repo) == 0 {
                    stop(repo, pipelines, mux.as_ref(), board.as_mut(), &mut out)?;
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

        // A pass that moved a task almost always has more ready right away,
        // so it runs the next pass straight off instead of waiting out the
        // rate — `skip_wait`, above, already bounds how long a streak of
        // these can run unbroken. Once it says no — nothing moved, or the
        // streak hit its ceiling — the count starts over from here.
        if worked {
            consecutive_working += 1;
            // The wait this pass is skipping is where the keyboard used to be
            // read, so a streak of up to `MAX_CONSECUTIVE_WORKING_PASSES`
            // passes left the board deaf between the first and the hundredth.
            // Reading here costs the streak nothing: this takes only what is
            // already sitting in the terminal's input buffer and returns at
            // once. The frame that goes with it is the one the top of the
            // next iteration draws, a moment from now.
            if let Some(board) = board.as_mut() {
                drain_keys(repo, pipelines, board, &mut stdin, &mut listening);
            }
            continue;
        }
        consecutive_working = 0;

        match board.as_mut() {
            // The wait: one frame per slice, and a slice never longer than
            // `status::POLL`. The board's own clocks — the lockup's frame,
            // the header's `up`, every running row's TIME — are all read at
            // draw time off the wall clock, so a board that does not draw
            // once a second stands still on screen while the run moves on
            // underneath it. `view::spool_frame` is the sharpest case: it
            // takes its frame from `secs % 2`, so at two draws per ten
            // seconds the lockup samples the same phase every time and stops
            // turning altogether.
            //
            // The watch stays inside the same `poll` — only the ceiling on a
            // single slice comes down. A keystroke or a file landing still
            // wakes the wait the instant it happens rather than at the end of
            // the second it landed in.
            //
            // A key applies to the board and the next slice's own draw shows
            // it. A queue change needs nothing beyond that draw, which
            // already rereads the queue fresh. A commands change is what a
            // finished background step is routed on, so that one breaks the
            // wait outright and lets the top of the outer loop run a fresh
            // pass at once.
            Some(board) => {
                // `stdin` and `listening`, declared once above rather than
                // here, so a descriptor the pass's own callback already
                // found gone this iteration is not asked again the moment
                // this wait starts — see that declaration's own comment.
                let until = std::time::Instant::now() + interval;
                'wait: while let Some(left) =
                    until.checked_duration_since(std::time::Instant::now())
                {
                    if crate::platform::stop::asked() {
                        break;
                    }
                    // Ahead of the block rather than after it, so the frame a
                    // slice is drawn for is up while that slice waits, not
                    // after it has already elapsed.
                    let _ = board.draw(repo, pipelines, crate::status::Phase::Waiting, &mut out);
                    let slice = crate::status::POLL.min(left);
                    let mut fds = Vec::new();
                    if listening {
                        fds.push(libc::STDIN_FILENO);
                    }
                    if let Some(watch) = &watch {
                        fds.push(watch.fd());
                    }
                    // Nothing to poll on: no watch this run could open, and
                    // stdin already found gone. The slice is the same second
                    // either way, spent asleep instead.
                    if fds.is_empty() {
                        std::thread::sleep(slice);
                        continue;
                    }

                    let ready = crate::screen::poll_ready(&fds, slice);
                    let mut idx = 0;
                    if listening {
                        if ready[idx] {
                            drain_keys(repo, pipelines, board, &mut stdin, &mut listening);
                        }
                        idx += 1;
                    }
                    if let Some(watch) = &watch
                        && ready[idx]
                    {
                        // Drained whatever this slice then decides to do
                        // with it. `DirWatch::drain` is the only read of the
                        // inotify
                        // fd, so a slice that polls it ready and leaves it
                        // unread leaves it ready: every slice after it would
                        // return from `poll_ready` at once and the wait would
                        // spin through the rest of its interval, drawing a
                        // full frame — and asking the multiplexer for its
                        // lane list — as fast as the terminal would take it.
                        let changed = watch.drain();
                        if changed.contains(&crate::screen::Changed::Commands) {
                            break 'wait;
                        }
                    }
                }
            }
            None => {
                println!(
                    "  next pass in {}",
                    crate::config::format_duration(interval)
                );
                let until = std::time::Instant::now() + interval;
                while let Some(left) = until.checked_duration_since(std::time::Instant::now()) {
                    if crate::platform::stop::asked() {
                        break;
                    }
                    let Some(watch) = &watch else {
                        // The floor alone, in the same slices as before —
                        // see `crate::screen::open_dir_watch`'s own doc —
                        // so a stop asked for mid-wait is still noticed
                        // within a second rather than a whole interval
                        // later.
                        std::thread::sleep(crate::status::POLL.min(left));
                        continue;
                    };
                    let ready = crate::screen::poll_ready(&[watch.fd()], left);
                    if ready[0] && watch.drain().contains(&crate::screen::Changed::Commands) {
                        break;
                    }
                }
            }
        }
    }
}

/// Whether the pass's own callback owes the board a frame: at once wherever a
/// key has just changed what one would show, and otherwise no faster than
/// [`crate::status::POLL`], the same cadence the wait draws at.
///
/// Its own function so the rule can be read and tested apart from the
/// closure it is used in — see the tests below.
fn tick_owes_a_frame(keyed: bool, since_last_frame: std::time::Duration) -> bool {
    keyed || since_last_frame >= crate::status::POLL
}

/// How many reads in a row may come back with nothing before the board stops
/// listening to stdin for the rest of one loop iteration.
///
/// [`crate::screen::read_key`] answers `None` both for a descriptor that has
/// gone away — a closed pipe, no controlling terminal — and for a read the
/// kernel interrupted, and nothing under `crate::screen` tells the two apart.
/// A gone descriptor polls "ready" forever and reads back empty every time,
/// so a single `None` used to latch the keyboard off for the iteration to
/// keep that from spinning down to nothing (jobs review finding 8). That also
/// left one interrupted read — a window resize, the stop handler firing —
/// costing the board every key for the rest of the iteration. Three in a row
/// still catch the spin within a handful of turns, and one on its own no
/// longer costs anything.
const EMPTY_READS_BEFORE_DEAF: u32 = 3;

/// Apply whatever is already sitting on stdin to `board`, and say whether any
/// of it changed what the next frame would draw.
///
/// Never waits: it reads only what is pending (`Duration::ZERO`), so it is
/// safe both between two passes of a streak that is deliberately not waiting
/// and inside a pass between its own units of work.
///
/// `listening` is this loop iteration's own answer to whether stdin is still
/// worth reading at all, and is cleared for good once
/// [`EMPTY_READS_BEFORE_DEAF`] reads in a row come back empty.
///
/// A key that fails goes to the problem log rather than nowhere. A `p` or an
/// `r` that could not be carried out used to be discarded here without a
/// word, so a keypress that failed looked exactly like one that was never
/// read at all. The board itself carries no trouble — see
/// [`crate::problem_log`] — which is why this goes to the log under it.
///
/// Takes any [`crate::screen::PollableRead`] rather than `RawStdin` itself,
/// the same seam [`crate::screen::read_key`] already reads through, so the
/// tests below can script the reads this has to get right: an empty read
/// that is only an interruption, and the run of them that means the
/// descriptor has gone.
fn drain_keys(
    repo: &Repo,
    pipelines: &Pipelines,
    board: &mut crate::status::Board,
    stdin: &mut impl crate::screen::PollableRead,
    listening: &mut bool,
) -> bool {
    let mut changed = false;
    let mut empty: u32 = 0;
    while *listening && stdin.byte_pending(std::time::Duration::ZERO) {
        match crate::screen::read_key(stdin) {
            Some(key) => {
                empty = 0;
                if let Err(err) = board.on_key(repo, pipelines, key) {
                    crate::problem_log::append(repo, &format!("board key {key:?}: {err:#}"));
                }
                changed = true;
            }
            None => {
                empty += 1;
                *listening = empty < EMPTY_READS_BEFORE_DEAF;
            }
        }
    }
    changed
}

/// The checklist's own opening lines — the banner and the "starting"
/// heading — pulled into its own function so [`dispatch`] and the 200ms
/// acceptance test below (`the_first_checklist_write_lands_within_200ms`)
/// run the exact same code, rather than the test re-typing a copy of it that
/// could drift from the real first-write path and stop proving anything.
fn print_checklist_header(out: &mut impl std::io::Write) -> Result<()> {
    write!(out, "{}", crate::status::banner("dispatch"))?;
    writeln!(out)?;
    writeln!(out, "  starting")?;
    Ok(())
}

/// The "starting" checklist's label column, sized to its own longest label
/// (`backend available`) rather than borrowed from an unrelated table such
/// as [`overview_cell`]'s — that one pads a queue row, not a check.
const CHECKLIST_LABEL_W: usize = 22;

fn checklist_label(label: &str) -> String {
    format!("{label:<CHECKLIST_LABEL_W$}")
}

/// The still-running mark — the mockup's own "the pending one is an
/// ellipsis", spelled out here as the real glyph rather than the two dots
/// the mockup falls back to only so its own doc stays readable in a plain
/// terminal (`fit` in `src/gate.rs` already uses this same character for the
/// same reason: an ellipsis, not three periods).
const CHECKLIST_PENDING: char = '…';

/// The done mark — the mockup's own "the `OK` column is a check mark", the
/// same glyph `status::view`'s own `Verdict::Pass` already draws for "this
/// passed" rather than a second one invented for this checklist alone.
const CHECKLIST_DONE: char = '✓';

/// Print a check's pending row — [`CHECKLIST_PENDING`], `label`, then what
/// it is waiting on — with no trailing newline, so [`checklist_clear`] can
/// wipe exactly this one line once the check returns. A no-op under
/// `--plain`: `checklist` is `board.is_some()` at the one call site in
/// [`dispatch`], and every checklist function here takes the same flag
/// rather than each re-deriving it.
fn checklist_pending(
    out: &mut impl std::io::Write,
    checklist: bool,
    label: &str,
    waiting: &str,
) -> Result<()> {
    if !checklist {
        return Ok(());
    }
    write!(
        out,
        "    {CHECKLIST_PENDING} {}{waiting}",
        checklist_label(label)
    )?;
    out.flush()?;
    Ok(())
}

/// Wipe the pending row [`checklist_pending`] printed, back to column zero —
/// `\x1b[2K` clears the whole line rather than only what is left of it, so a
/// short done row written over a longer pending one leaves nothing behind.
fn checklist_clear(out: &mut impl std::io::Write, checklist: bool) -> Result<()> {
    if !checklist {
        return Ok(());
    }
    write!(out, "\r\x1b[2K")?;
    Ok(())
}

/// Print a check's own [`CHECKLIST_DONE`] row, once it has returned
/// successfully — `detail` is whatever the mockup draws beside it, or empty
/// for the checks that draw nothing there.
fn checklist_done(
    out: &mut impl std::io::Write,
    checklist: bool,
    label: &str,
    detail: &str,
) -> Result<()> {
    if !checklist {
        return Ok(());
    }
    match detail.is_empty() {
        true => writeln!(out, "    {CHECKLIST_DONE} {label}")?,
        false => writeln!(
            out,
            "    {CHECKLIST_DONE} {}{detail}",
            checklist_label(label)
        )?,
    }
    Ok(())
}

/// One row of the "starting" checklist: a pending line naming what `check`
/// is waiting on, replaced by its done row the moment it returns — or, on
/// failure, left as a bare newline so the error `?` propagates lands on a
/// fresh line rather than run into the pending text.
///
/// Only for a check whose own failure must abort the whole start — see the
/// workspace-open row in [`dispatch`], which prints its pending and cleared
/// rows the same way but by hand, because its own failure is reported and
/// stepped over instead of raised.
fn checklist_row<T>(
    out: &mut impl std::io::Write,
    checklist: bool,
    label: &str,
    waiting: &str,
    check: impl FnOnce() -> Result<T>,
) -> Result<T> {
    checklist_pending(out, checklist, label, waiting)?;
    match check() {
        Ok(value) => {
            checklist_clear(out, checklist)?;
            Ok(value)
        }
        Err(err) => {
            if checklist {
                writeln!(out)?;
            }
            Err(err)
        }
    }
}

/// The unique `pipeline:` names a batch of live tasks routes through, in the
/// order each is first seen — the "starting" checklist's own detail for
/// `task routes`, drawn from the same tasks [`check_task_routes`] just
/// validated rather than a second read of the queue.
fn task_route_names(tasks: &[Task]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for task in tasks {
        if let Some(name) = task.front.pipeline.as_deref()
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names.join(", ")
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

/// The queue overview: every task `Repo::tasks()` holds right now, grouped
/// by `group:` — the mockup on task `overview-and-gates`. `Ok(true)` to go
/// on to the overrides gate, `Ok(false)` only for `esc`.
///
/// A thin wrapper over [`overview_gate_with`] — see [`overrides_gate`]'s own
/// doc comment for why this split exists at all; the two gates share it for
/// the same reason.
///
/// `own_term` is false whenever `dispatch`'s own board is already holding
/// the terminal — see its own doc comment on the call site for why a second,
/// nested `TermGuard` here would be a bug.
fn overview_gate(repo: &Repo, own_term: bool) -> Result<bool> {
    overview_gate_with(
        repo,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        own_term.then_some(crate::platform::TermGuard::new as fn() -> _),
    )
}

/// [`overview_gate`]'s own logic. Never printed for a non-interactive run:
/// unlike the overrides notice, there is no record this needs to leave in a
/// log nobody is watching — it is a person's own screen, or nothing.
///
/// `term: None` for a caller that already holds a [`crate::platform::TermGuard`]
/// of its own — `commands::queue::confirm_start`, reusing the one
/// `run_screen` holds for its whole session — and `Some` for one that does
/// not, taken only just before the first blocking read: see
/// `commands::queue::tool_requirements_gate_with`'s own doc on why a second,
/// nested guard is a bug rather than merely redundant.
pub(crate) fn overview_gate_with(
    repo: &Repo,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    if !interactive {
        return Ok(true);
    }

    let tasks = repo.tasks()?;
    let _term = term.map(|term| term());
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in overview_lines(&tasks, None) {
        writeln!(out, "{line}")?;
    }
    loop {
        match crate::screen::read_key(input) {
            Some(crate::screen::Key::Enter) => return Ok(true),
            Some(crate::screen::Key::Esc) => return Ok(false),
            // The tty went away mid-question, or — reached through
            // `commands::queue::confirm_start` — the script driving the
            // queue screen simply ran out of keys, exactly the way it ends
            // every other mode: see `queue_screen`'s own doc comment on why
            // an exhausted pipe reads as `esc` rather than as a leftover
            // key nobody typed. `overrides_gate_with`'s own copy of this
            // match proceeds instead on the same read — this is a new
            // screen weighing a new decision, so it takes the more
            // conservative of the two rather than inheriting that one's.
            None => return Ok(false),
            _ => {}
        }
    }
}

/// The same overview, drawn for the one thing left to do with it once
/// `commands::queue::after_write` finds the queue's lock already held: join
/// the run that is already going rather than start one that cannot. `Ok(true)`
/// for `enter`, meaning the caller should now focus that dispatcher's
/// workspace and let this screen end; `Ok(false)` for `esc`, back to
/// browsing, and for the tty going away mid-question — the same
/// conservative reading [`overview_gate_with`] gives that case.
///
/// Always interactive and always `term: None`: the only caller is
/// `commands::queue::begin_submission`, reached from `run_screen`, which
/// already holds its own `TermGuard` — see [`overview_gate_with`]'s own doc
/// comment on why a second one here would be a bug rather than merely
/// redundant.
pub(crate) fn dispatcher_running_gate_with(
    repo: &Repo,
    pid: u32,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<bool> {
    let tasks = repo.tasks()?;
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in overview_lines(&tasks, Some(pid)) {
        writeln!(out, "{line}")?;
    }
    loop {
        match crate::screen::read_key(input) {
            Some(crate::screen::Key::Enter) => return Ok(true),
            Some(crate::screen::Key::Esc) => return Ok(false),
            None => return Ok(false),
            _ => {}
        }
    }
}

/// The overview's own column widths, sized to the mockup's own longest
/// example row: the task id, pipeline and step columns are fixed, and
/// `OVERVIEW_BASE_W` is whatever is left of 80 columns once the two-space
/// indent and the other three have taken their share — the one column with
/// no ceiling of its own otherwise, since a base names a branch and nothing
/// stops a branch running long.
const OVERVIEW_NAME_W: usize = 20;
const OVERVIEW_PIPELINE_W: usize = 12;
const OVERVIEW_STEP_W: usize = 10;
const OVERVIEW_BASE_W: usize = 80 - 2 - OVERVIEW_NAME_W - OVERVIEW_PIPELINE_W - OVERVIEW_STEP_W;

/// One cell of the overview's table: `queue::clip`'s own ellipsis-cut, then
/// padded out to `width` — the same combination the trial picker's own id
/// column already uses (`assign_pipelines_panel`), so a task id, pipeline
/// name or step id too long for its column is cut rather than pushing every
/// column after it out past 80 (review finding 1). Clips to `width - 1`
/// rather than `width`: `assign_pipelines_panel` clips to a width derived
/// from the longest id and then writes its own explicit separator after it,
/// so its cells never touch, but this table has no separate separator —
/// clipping to the full column width let a cell exactly as long as its
/// column run straight into the next one with no gap (review finding 1,
/// still open after the first fix). Reserving one column of the budget for
/// the gap keeps every row at exactly 80 columns while leaving at least one
/// space before the next column, matching the mockup's own gapped layout.
fn overview_cell(text: &str, width: usize) -> String {
    crate::screen::pad_to(&super::queue::clip(text.to_string(), width - 1), width)
}

/// The overview's own lines, grouped by `group:` and sorted by group name —
/// a task naming none is a group of one, keyed by its own id, the same
/// reading [`crate::status::Row::group`] gives it. Pure, so a caller never
/// has to reach past `repo.tasks()` to draw the exact screen the mockup
/// draws — every task the queue directory holds, never the archive, since
/// `Repo::tasks` never reads that directory at all.
///
/// `held_pid` is `None` for the ordinary "about to start one" draw, and the
/// running dispatcher's own pid once `commands::queue::after_write` finds
/// the lock already held — see the `focus-live-run` mockup, which draws
/// both the pid line under the header and the swapped footer this same
/// table then carries, rather than a screen of its own: nothing about the
/// board changes, only what a person can do once they are looking at it.
fn overview_lines(tasks: &[Task], held_pid: Option<u32>) -> Vec<String> {
    let mut by_group: std::collections::BTreeMap<&str, Vec<&Task>> = Default::default();
    for task in tasks {
        let key = task.front.group.as_deref().unwrap_or(task.id());
        by_group.entry(key).or_default().push(task);
    }

    let mut lines = vec![format!(
        "queued  {} · {}",
        plural(by_group.len(), "group"),
        plural(tasks.len(), "task")
    )];
    if let Some(pid) = held_pid {
        lines.push(format!(
            "a dispatcher is already running (pid {pid}) — it takes these on its next pass"
        ));
    }
    lines.push(String::new());
    lines.push(format!(
        "  {}{}{}{}",
        overview_cell("TASK", OVERVIEW_NAME_W),
        overview_cell("PIPELINE", OVERVIEW_PIPELINE_W),
        overview_cell("STEP", OVERVIEW_STEP_W),
        "BASE"
    ));
    for group_tasks in by_group.values() {
        let name = group_tasks[0]
            .front
            .group
            .as_deref()
            .unwrap_or(group_tasks[0].id());
        lines.push(String::new());
        lines.push(super::queue::clip(name.to_string(), 80));
        for task in group_tasks {
            lines.push(format!(
                "  {}{}{}{}",
                overview_cell(task.id(), OVERVIEW_NAME_W),
                overview_cell(
                    task.front.pipeline.as_deref().unwrap_or("—"),
                    OVERVIEW_PIPELINE_W
                ),
                overview_cell(task.stage(), OVERVIEW_STEP_W),
                super::queue::clip(
                    task.front.base.as_deref().unwrap_or("—").to_string(),
                    OVERVIEW_BASE_W
                ),
            ));
        }
    }
    lines.push(String::new());
    lines.push(if held_pid.is_some() {
        "[enter] go to the dispatcher   [esc] back".to_string()
    } else {
        "[enter] start a dispatcher   [esc] back".to_string()
    });
    lines
}

/// `n` with its noun, singular where that is what `n` is — [`overview_lines`]'s
/// own copy of the same rule `commands::queue::plural` already applies to the
/// pending screen, kept local rather than shared across the two: neither
/// module is the other's to reach into for one line of pluralization.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// The standing consent gate for a patch layer (see [`crate::overrides`]):
/// `Ok(true)` to go on and start the run, `Ok(false)` only for `esc`, the one
/// path that must reach the caller before `Lock::acquire` runs at all.
///
/// A thin wrapper over [`overrides_gate_with`], which does the real work
/// against an injected reader, writer and terminal guard — this is the only
/// thing that touches the process's real stdio, the same split
/// `commands::queue::queue_screen` draws around `run_screen`. `TermGuard::new`
/// is handed over as a factory rather than constructed here: raw mode is a
/// presentation detail for the one branch that actually blocks on a
/// keypress, and building it eagerly printed `hide_cursor`'s escape into
/// every single dispatch, layered or not, tty or not (finding: `hide_cursor`
/// has no `is_terminal` guard of its own, unlike `raw_mode` and
/// `drain_stdin`). A test hands over `TermGuard::inert` instead, so it never
/// fights another test over the real terminal (see `TermGuard`'s own `inert`
/// field) even while driving the branch that would otherwise construct one.
///
/// `own_term` is false whenever `dispatch`'s own board is already holding
/// the terminal — see [`overview_gate`]'s own doc comment on the pair.
fn overrides_gate(repo: &Repo, own_term: bool) -> Result<bool> {
    overrides_gate_with(
        repo,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        own_term.then_some(crate::platform::TermGuard::new as fn() -> _),
    )
}

/// [`overrides_gate`]'s own logic, taking whether anyone is there to answer,
/// where the notice and the prompt go, and how to take the terminal for the
/// one branch that reads a key — so a test can drive every branch, the
/// no-tty print-and-proceed path included, without a real terminal at all.
///
/// Nothing here may print the interactive prompt and then block: with a
/// layer present but no terminal on both ends — every unattended run, and
/// the e2e suites that never allocate one — the notice still goes to the
/// log, because the layer is otherwise invisible in a `git status` and a run
/// nobody is watching is exactly the one that most needs it on record, but
/// the run proceeds without waiting on an answer nobody can give. With a
/// layer already acknowledged and unmoved since, this says nothing at all —
/// "don't ask again until this changes" means exactly that.
///
/// `term: None` for a caller that already holds a [`crate::platform::TermGuard`]
/// of its own — see [`overview_gate_with`]'s own doc comment on the pair, and
/// `commands::queue::tool_requirements_gate_with` on why a second, nested
/// guard is a bug rather than merely redundant.
pub(crate) fn overrides_gate_with(
    repo: &Repo,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    let rows = collect_override_rows(&repo.overrides_dir())?;
    if rows.is_empty() {
        return Ok(true);
    }

    if !interactive {
        print_overrides_notice(out, &rows)?;
        return Ok(true);
    }

    // The layer's own fingerprint — a tracked-file edit alone must not
    // reopen a gate the layer itself has not moved. `unwrap_or_default`
    // only stands in for the moment between
    // `rows` being non-empty and the same directory being read a second
    // time; an empty string never equals a real fingerprint, so this still
    // asks rather than silently trusting a layer it could not re-read.
    let fingerprint = crate::version::layer_fingerprint(repo).unwrap_or_default();
    if !crate::overrides::ack_needed(&repo.home, &fingerprint) {
        return Ok(true);
    }

    // Taken only now, right before the first read that can actually block —
    // every early return above constructs no guard at all, so a dispatch
    // with no layer, or one already acknowledged, hides and shows nothing.
    let _term = term.map(|term| term());
    let _ = write!(out, "\x1b[2J\x1b[H");
    print_overrides_notice(out, &rows)?;
    writeln!(
        out,
        "  [enter] start the run   [esc] back   [x] don't ask again until this changes"
    )?;

    loop {
        match crate::screen::read_key(input) {
            Some(crate::screen::Key::Enter) => return Ok(true),
            Some(crate::screen::Key::Esc) => return Ok(false),
            Some(crate::screen::Key::Char('x' | 'X')) => {
                crate::overrides::ack_write(&repo.home, &fingerprint)?;
                return Ok(true);
            }
            // The tty went away mid-question — a closed pane, say. Nothing
            // here may hang waiting for an answer that can no longer come.
            None => return Ok(true),
            _ => {}
        }
    }
}

/// The layer's own summary, one line per overridden artifact — the mockup's
/// "N keys" / "whole file" column, derived from [`OverrideRow`] rather than
/// `override list`'s own `patch` / `whole file` kind, since a person reading
/// this notice wants to know how much changed, not what shape the file is.
fn print_overrides_notice(out: &mut impl std::io::Write, rows: &[OverrideRow]) -> Result<()> {
    writeln!(out)?;
    writeln!(out, "  overrides are active for this project")?;
    writeln!(out)?;
    let target_w = rows.iter().map(|r| r.target.len()).max().unwrap_or(0);
    let labels: Vec<String> = rows.iter().map(overrides_gate_kind).collect();
    let kind_w = labels.iter().map(String::len).max().unwrap_or(0);
    for (row, kind) in rows.iter().zip(&labels) {
        if row.overrides == "—" {
            writeln!(out, "    {:<target_w$}  {kind}", row.target)?;
        } else {
            writeln!(
                out,
                "    {:<target_w$}  {kind:<kind_w$}  {}",
                row.target, row.overrides
            )?;
        }
    }
    writeln!(out)?;
    Ok(())
}

/// "N keys" for a pipeline or config patch, "whole file" for a prompt.
fn overrides_gate_kind(row: &OverrideRow) -> String {
    if row.kind != "patch" {
        return row.kind.to_string();
    }
    let n = row.overrides.split(", ").filter(|k| !k.is_empty()).count();
    format!("{n} key{}", if n == 1 { "" } else { "s" })
}

/// The screen between the overrides gate and the run itself: doctor's cheap
/// findings (see [`crate::commands::doctor::cheap_findings`]) under the
/// mockup's own three headings — one of the two notices [`dispatch`] used to
/// print with a bare `println!` and lose to `Board::draw`'s own clear screen
/// a moment later (task `warnings-screen`).
///
/// An unattended run with no ceiling gets no block of its own here any
/// more: doctor's own `unattended.enabled` note (see
/// `pipeline_graph_checks`) already lands under `settings`, since both read
/// the same string. It fires on `config.unattended.enabled` alone and
/// cannot see a `--unattended` flag with the config setting left off — an
/// accepted gap, not one this screen closes.
///
/// `Ok(true)` to go on and start the run, `Ok(false)` only for `esc`.
/// Alongside [`overview_gate`] and [`overrides_gate`], and — like both —
/// before `Lock::acquire`: `esc` here must still mean "nothing has happened
/// yet", which is only true ahead of the lock. The other notice this task
/// exists to fix, a failure to open the run's shared workspace, cannot join
/// this screen for exactly that reason — it is only attempted once the lock
/// is held — so it gets its own, smaller one instead; see
/// [`workspace_open_notice`].
///
/// `own_term` is false whenever `dispatch`'s own board is already holding
/// the terminal — see [`overview_gate`]'s own doc comment on the pair.
fn warnings_gate(repo: &Repo, pipelines: &Pipelines, own_term: bool) -> Result<bool> {
    warnings_gate_with(
        repo,
        pipelines,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        own_term.then_some(crate::platform::TermGuard::new as fn() -> _),
    )
}

/// [`warnings_gate`]'s own logic, against an injected reader, writer and
/// terminal guard — see [`overrides_gate_with`]'s own doc comment on why the
/// split exists and what `term: None` means to a caller that already holds
/// a guard.
///
/// Skipped entirely — no draw, no fingerprint, `Ok(true)` at once — when
/// none of the three sections has anything in it. With something to say but
/// no tty on either end, the notice is still printed, once, so an unattended
/// run leaves the same record in its log that `overrides_gate_with` leaves
/// for its own notice; nothing here may then block on a keypress nobody can
/// answer.
pub(crate) fn warnings_gate_with(
    repo: &Repo,
    pipelines: &Pipelines,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    use crate::commands::doctor::Warning;

    let cheap = crate::commands::doctor::cheap_findings(repo, pipelines, &repo.config);
    let settings: Vec<String> = cheap
        .iter()
        .filter_map(|w| match w {
            Warning::Setting(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let files: Vec<String> = cheap
        .iter()
        .filter_map(|w| match w {
            Warning::File(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let problems: Vec<String> = cheap
        .iter()
        .filter_map(|w| match w {
            Warning::Problem(text) => Some(text.clone()),
            _ => None,
        })
        .collect();

    if settings.is_empty() && files.is_empty() && problems.is_empty() {
        return Ok(true);
    }

    if !interactive {
        print_warnings_notice(out, &settings, &files, &problems)?;
        return Ok(true);
    }

    // Fingerprinted on the body alone — never the header or the footer, both
    // of which are this screen's own wording rather than a fact about the
    // project — so a person who has hidden this exact set of lines is not
    // asked again merely because a later spoolway rewords the prompt beneath
    // them.
    let rendered = warnings_lines(&settings, &files, &problems).join("\n");
    let fingerprint = crate::skeleton::fingerprint(&rendered);
    if !crate::overrides::warnings_ack_needed(&repo.home, &fingerprint) {
        return Ok(true);
    }

    let _term = term.map(|term| term());
    let _ = write!(out, "\x1b[2J\x1b[H");
    print_warnings_notice(out, &settings, &files, &problems)?;
    // Flush left, in the same column as the headings above it — the
    // mockup's own footer, unlike the overrides gate's, sits under a body
    // that is not itself indented two columns.
    writeln!(
        out,
        "[enter] start the run   [esc] back   [x] hide until these change"
    )?;

    loop {
        match crate::screen::read_key(input) {
            Some(crate::screen::Key::Enter) => return Ok(true),
            Some(crate::screen::Key::Esc) => return Ok(false),
            Some(crate::screen::Key::Char('x' | 'X')) => {
                crate::overrides::warnings_ack_write(&repo.home, &fingerprint)?;
                return Ok(true);
            }
            // The tty went away mid-question. Nothing here may hang waiting
            // for an answer that can no longer come — see
            // `overrides_gate_with`'s own copy of this same reasoning.
            None => return Ok(true),
            _ => {}
        }
    }
}

/// The one screen [`warnings_gate`] cannot show: a failure to find or open
/// this run's own shared workspace, only known once `dispatch` has already
/// taken the lock and attempted it — see that call site's own comment.
/// Unlike `warnings_gate`, there is no earlier screen left to decline back
/// to here, so `[enter]` is the only key this reads, and nothing is
/// fingerprinted: opening a workspace either works or it does not, once,
/// this run — there is no standing state worth hiding until it changes.
///
/// `own_term` is false whenever `dispatch`'s own board is already holding
/// the terminal — see [`overview_gate`]'s own doc comment on the pair. This
/// is a fourth gate-shaped screen reached the same way the other three are,
/// so it needs the same guard.
fn workspace_open_notice(err: &str, own_term: bool) -> Result<()> {
    workspace_open_notice_with(
        err,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        own_term.then_some(crate::platform::TermGuard::new as fn() -> _),
    )
}

/// [`workspace_open_notice`]'s own logic, against an injected reader, writer
/// and terminal guard — see [`overrides_gate_with`]'s own doc comment on the
/// pattern. With no tty on either end the notice is still printed, once, so
/// it is on record; nothing here may then block on a keypress nobody can
/// answer.
pub(crate) fn workspace_open_notice_with(
    err: &str,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<()> {
    let problems = [err.to_string()];
    if !interactive {
        print_warnings_notice(out, &[], &[], &problems)?;
        return Ok(());
    }

    let _term = term.map(|term| term());
    let _ = write!(out, "\x1b[2J\x1b[H");
    print_warnings_notice(out, &[], &[], &problems)?;
    writeln!(out, "[enter] continue")?;

    loop {
        match crate::screen::read_key(input) {
            Some(crate::screen::Key::Enter) | None => return Ok(()),
            _ => {}
        }
    }
}

/// The warnings screen's own body: the mockup's heading, then whichever of
/// `settings`/`files`/`problems` has anything to say, in that order, each
/// under its own heading and skipped entirely when empty — the whole reason
/// the screen it backs is skipped too when all three are.
fn print_warnings_notice(
    out: &mut impl std::io::Write,
    settings: &[String],
    files: &[String],
    problems: &[String],
) -> Result<()> {
    writeln!(out, "before this run starts")?;
    writeln!(out)?;
    for line in warnings_lines(settings, files, problems) {
        writeln!(out, "{line}")?;
    }
    writeln!(out)?;
    Ok(())
}

/// [`print_warnings_notice`]'s own lines, without the leading title or the
/// trailing blank line, pulled out on its own so the fingerprint gate reads
/// exactly the text a person was shown — never the title, which never
/// changes, and never the footer, which is this screen's prompt rather than
/// a fact about the project.
///
/// One line per item, with nothing between them: a scan of names before
/// pressing `enter` has no room for a blank line every notice used to carry.
fn warnings_lines(settings: &[String], files: &[String], problems: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    for (heading, texts) in [
        ("settings", settings),
        ("files", files),
        ("problems", problems),
    ] {
        if texts.is_empty() {
            continue;
        }
        lines.push(heading.to_string());
        for text in texts {
            lines.extend(wrap_indent(text, "  ", 80));
        }
        lines.push(String::new());
    }
    // The loop above leaves one trailing blank line behind its last
    // section — wanted between sections, not after all of them, where
    // `print_warnings_notice` already puts its own blank line before the
    // footer.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// `text`, word-wrapped to `width` columns, `indent` at the start of the
/// first line and two columns further in on every line after it — the
/// mockup's own hang, telling a wrapped continuation apart from the next
/// item without a blank line to separate them now that [`warnings_lines`]
/// prints none. A local copy of the same wrapping `commands::queue`'s own
/// `wrapped` does for a labeled row, rather than a reach into a sibling
/// module for one small utility this screen has no label to sit beside.
fn wrap_indent(text: &str, indent: &str, width: usize) -> Vec<String> {
    let hang = format!("{indent}  ");
    let mut lines = Vec::new();
    let mut current = indent.to_string();
    let mut bare = true;
    for word in text.split_whitespace() {
        if !bare && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::replace(&mut current, hang.clone()));
            bare = true;
        }
        if !bare {
            current.push(' ');
        }
        current.push_str(word);
        bare = false;
    }
    lines.push(current);
    lines
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

    // Asked the way a commit asks it. `git var` builds the identity from
    // the environment first — `GIT_AUTHOR_NAME`, `GIT_COMMITTER_EMAIL`,
    // `EMAIL` — then config, honouring `user.useConfigOnly`, and fails
    // exactly when a commit would. A CI runner or container that supplies
    // its identity that way has no `user.name` to read and every commit
    // succeeds; `git config` alone refused it.
    if repo.git(&["var", "GIT_AUTHOR_IDENT"]).is_ok()
        && repo.git(&["var", "GIT_COMMITTER_IDENT"]).is_ok()
    {
        return Ok(None);
    }

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
        // Both keys read back, and git still cannot build an identity from
        // them — `user.useConfigOnly` with a value git rejects, say. Git's
        // own words are the ones to act on.
        return Err(anyhow::Error::new(Refusal {
            reason: format!("git cannot build a commit identity, and {reason}"),
            fix: "the first commit would fail. Run `git var GIT_COMMITTER_IDENT` in the \
                  checkout to see what git objects to, and fix that"
                .to_string(),
        }));
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
/// between one dispatch pass and the next. `Herdr::new` itself no longer
/// resolves or falls back through `main_checkout` at all — it takes
/// `repo.checkout` exactly as handed to it, see `Herdr::anchor`'s doc in
/// `src/mux.rs` — so this check is the one place either of the following is
/// caught, not a second opinion on something `Herdr::new` also checks:
///
/// - `repo.root` has no main checkout `main_checkout` can place at all — a
///   bare repository, or a `.git` too unusual for it to place.
/// - Under `MuxMode::Split` (`Mux::task_owns_workspace`) only: `repo.checkout`
///   — the checkout the dispatcher actually ran in, and what `Herdr` hands
///   herdr as `--cwd` — is itself a linked worktree. herdr refuses a `--cwd`
///   that is a linked worktree with `linked_worktree_source` (verified
///   against a live herdr; see `Herdr::anchor`'s doc), and only `split`'s own
///   routes hand that failure nowhere to fall back to. `grouped` is not
///   refused this same checkout — see
///   `backend_checkout_passes_herdr_on_a_dispatcher_started_in_a_linked_worktree_under_grouped_mode`
///   in this module's tests for why.
pub(crate) fn check_backend_checkout(
    repo: &Repo,
    mux: &dyn crate::mux::Mux,
) -> Result<Option<String>> {
    if !mux.is_available() {
        bail!("{}", mux.unavailable());
    }
    if mux.name() == "herdr" {
        if crate::repo::main_checkout(&repo.root).is_none() {
            return Err(anyhow::Error::new(Refusal {
                reason: format!(
                    "{} has no main checkout herdr can resolve a workspace onto",
                    repo.root.display()
                ),
                fix: "a bare repository, say. Switch backends:\n\n  spoolway config set \
                      dispatch.backend headless"
                    .to_string(),
            }));
        }
        // Only `MuxMode::Split` (`Mux::task_owns_workspace`) ever hands
        // herdr this checkout as `--cwd` at all: `MuxMode::Grouped`'s own
        // per-task route, `project_tab` in `src/dispatch.rs`, takes the
        // `false` arm of `task_owns_workspace` unconditionally — borrowed
        // checkout or freshly cut, `grouped` never calls `Mux::create_pane`
        // or reaches `open_worktree_workspace` from the ordinary dispatch
        // loop at all. `split`'s own first cut, `Mux::create_workspace`,
        // propagates a refused anchor with no fallback; `split`'s other two
        // routes, `Mux::create_pane` (a borrowed checkout) and the default
        // `Mux::reopen_owned_pane` (which is exactly `create_pane`), each
        // already fall back to a plain `workspace create` when herdr
        // refuses the anchor they tried first — but a checkout the anchor
        // is genuinely wrong for is refused here regardless of which of the
        // three a given task would have hit, so every `split` task started
        // on it fails or degrades the same way rather than some of them
        // working oddly while others crash. Refusing `grouped` the same
        // checkout would be a new failure it never had, not the bug this
        // task fixes.
        //
        // A linked worktree's own top always carries a `.git` *file*
        // pointing at the common git dir; the main checkout's is a
        // directory. The same test herdr itself makes of `--cwd`.
        if mux.task_owns_workspace() && repo.checkout.join(".git").is_file() {
            let main = crate::repo::main_checkout(&repo.checkout).unwrap_or(repo.root.clone());
            return Err(anyhow::Error::new(Refusal {
                reason: format!(
                    "herdr cannot open a workspace on this checkout\n  dispatching from  {}\n  \
                     main checkout     {}",
                    repo.checkout.display(),
                    main.display()
                ),
                fix: format!(
                    "herdr refuses a linked worktree as a workspace root, so every lane \
                     would be filed under {} instead.\n\n  run the dispatcher from {}",
                    main.display(),
                    main.display()
                ),
            }));
        }
    }
    Ok(Some(format!("{} · main checkout", mux.name())))
}

/// Refuse a start nobody can see: a herdr run outside any pane, or a
/// `backend = headless` run started without the end-to-end harness's own
/// marker.
///
/// Not a third [`Refusal`]: neither half of this reads as a `doctor` row —
/// there is no dispatcher yet for `doctor` to ask whether it is visible —
/// and the herdr half's own message is the multi-line shape the task's
/// mockup draws, not the single line [`refuse`] joins a reason and a fix
/// into.
///
/// `headless` is checked by name rather than by a trait method the way the
/// herdr half is: the marker gates the backend itself, not any property a
/// `Mux` could answer for — a fake pane to ask about would be one more thing
/// for a test double to get right for no reason a real backend needs.
fn check_dispatcher_visible(mux: &dyn crate::mux::Mux) -> Result<()> {
    if mux.name() == "headless" {
        if crate::platform::env_var(crate::headless::TEST_BACKEND_ENV).is_err() {
            bail!(
                "backend = headless is spoolway's own test backend — nothing draws it \
                 anywhere a person can see, so only the end-to-end harness runs it, with \
                 {} exported.\n\n  Switch back:\n\n    spoolway config set dispatch.backend \
                 herdr",
                crate::headless::TEST_BACKEND_ENV
            );
        }
        return Ok(());
    }
    if !mux.in_own_pane() {
        bail!(
            "a dispatcher has to be visible, and this is not a herdr pane.\n\n  Open one and \
             run it there:\n\n    herdr\n    spoolway dispatch"
        );
    }
    Ok(())
}

/// Refuse the whole start over any live task whose `pipeline:` does not
/// resolve — absent, or naming a pipeline this project does not define.
/// There is no project default any more, so a task that cannot route here
/// never will on its own; refusing before the lock and before any lane
/// starts is what keeps that from being found only once a lane tries to run
/// it, on whichever task happens to reach it first.
///
/// Deliberately its own function rather than a third [`Refusal`] fed through
/// [`refuse`]: `doctor` has no use for this one — a task file is not a
/// project setting for it to hold a row open on — and the mockup this task
/// draws is its own multi-line shape, not the single line `refuse` joins a
/// [`Refusal`]'s `reason` and `fix` into.
fn check_task_routes(pipelines: &Pipelines, tasks: &[Task]) -> Result<()> {
    let choices = pipelines.names().join(", ");
    for task in tasks {
        match task.front.pipeline.as_deref().map(str::trim) {
            None | Some("") => bail!(
                "refusing to start: task `{}` has no `pipeline:`\n  Set `pipeline:` to one of: \
                 {choices}.\n\nNothing was dispatched.",
                task.id()
            ),
            Some(name) if pipelines.get(name).is_err() => bail!(
                "refusing to start: task `{}` names a pipeline that does not exist: `{name}`\n  \
                 Set `pipeline:` to one of: {choices}.\n\nNothing was dispatched.",
                task.id()
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

/// A repo whose lock another process already holds: name it, and, for a
/// person actually looking at a terminal, bring the pane it is drawing its
/// board in to the front — there is one board per run now, and it is
/// already up.
///
/// `--plain` keeps its own one-shot table, byte for byte: a script asking
/// what is running gets an answer meant for parsing, and "focusing its pane"
/// is a line for a person, not a caller polling this in a loop.
fn already_running(
    repo: &Repo,
    pipelines: &Pipelines,
    pid: u32,
    args: &DispatchArgs,
    out: &mut impl std::io::Write,
) -> Result<()> {
    if args.plain {
        // One read, one print: a script wants the table as it stands, not a
        // process that sits there polling on its behalf.
        writeln!(out, "watching dispatcher (pid {pid})\n")?;
        let rows = crate::status::rows(repo, pipelines)?;
        write!(out, "{}", crate::status::plain_table(&rows))?;
        return Ok(());
    }

    writeln!(
        out,
        "  a dispatcher is already running for this repo (pid {pid})"
    )?;
    // `None` for an older three-line lock, or a run with no pane recorded —
    // nothing here to focus, so nothing more is printed. A pane that has
    // gone away since the lock was written is not fatal either: reported and
    // stepped over, the same as a failed workspace move is today.
    if let Some(pane_id) = crate::lock::Lock::pane(&repo.lock_file()) {
        match crate::mux::backend(repo).and_then(|mux| mux.focus_pane(&pane_id)) {
            Ok(()) => writeln!(out, "  → focusing its pane {pane_id}")?,
            Err(err) => writeln!(out, "  → could not focus its pane {pane_id}: {err:#}")?,
        }
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
) -> Result<()> {
    let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux);
    let mut report = crate::dispatch::Report::default();
    match dispatcher.sweep_on_stop(&mut report) {
        Ok(()) => {
            for action in &report.actions {
                println!("  {action}");
            }
        }
        Err(err) => println!("  ! could not settle this run's lanes: {err:#}"),
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

    /// A pass draws on the same once-a-second cadence the wait does, so the
    /// lockup keeps turning and the clocks keep moving while a pass spends
    /// whole seconds cutting a worktree or opening a pane.
    #[test]
    fn a_pass_owes_a_frame_once_a_second_and_no_faster() {
        use std::time::Duration;

        assert!(
            !tick_owes_a_frame(false, Duration::ZERO),
            "two tick points in the same instant are one frame, not two"
        );
        assert!(
            !tick_owes_a_frame(false, crate::status::POLL - Duration::from_millis(1)),
            "just under the cadence still owes nothing"
        );
        assert!(
            tick_owes_a_frame(false, crate::status::POLL),
            "a second since the last frame owes the next one"
        );
    }

    /// A key that changed the board is drawn straight away rather than at the
    /// next second: a person who pressed something must not wait out the rest
    /// of the pass to see it land.
    #[test]
    fn a_key_owes_a_frame_whatever_the_clock_says() {
        assert!(tick_owes_a_frame(true, std::time::Duration::ZERO));
    }

    /// Stdin as a script: what each read answers, oldest first. `Some(byte)`
    /// is a byte a key decodes from; `None` is a read that came back with
    /// nothing, which is what [`crate::screen::read_key`] reports both for an
    /// interrupted read and for a descriptor that has gone away.
    ///
    /// `byte_pending` answers for whatever is left of the script, so a
    /// scripted empty read at the very end reads as "nothing more waiting"
    /// — an interruption on an otherwise live terminal — while a run of them
    /// reads as a descriptor that keeps polling ready and keeps coming back
    /// empty, the shape [`EMPTY_READS_BEFORE_DEAF`] is there for.
    struct ScriptedStdin(std::collections::VecDeque<Option<u8>>);

    impl ScriptedStdin {
        fn new(reads: impl IntoIterator<Item = Option<u8>>) -> ScriptedStdin {
            ScriptedStdin(reads.into_iter().collect())
        }
    }

    impl std::io::Read for ScriptedStdin {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.pop_front() {
                Some(Some(byte)) => {
                    buf[0] = byte;
                    Ok(1)
                }
                // An empty read: `Ok(0)`, the same answer a closed
                // descriptor gives.
                _ => Ok(0),
            }
        }
    }

    impl crate::screen::PollableRead for ScriptedStdin {
        fn byte_pending(&self, _timeout: std::time::Duration) -> bool {
            !self.0.is_empty()
        }
    }

    /// One empty read is an interruption — a window resize, the stop handler
    /// firing — not a descriptor that has gone. The board keeps listening,
    /// and the key typed after it still lands.
    #[test]
    fn a_lone_empty_read_leaves_the_board_still_listening() {
        let repo = fixture("drain-keys-one-empty-read");
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::for_test();
        let mut stdin = ScriptedStdin::new([None, Some(b'x')]);
        let mut listening = true;

        let changed = drain_keys(&repo, &pipelines, &mut board, &mut stdin, &mut listening);

        assert!(listening, "one empty read must not cost the keyboard");
        assert!(
            changed,
            "the key behind the empty read still reached the board"
        );
    }

    /// A descriptor that has gone away polls ready for ever and reads back
    /// empty every time. [`EMPTY_READS_BEFORE_DEAF`] in a row is what stops
    /// the board spinning on it for the rest of the loop iteration.
    #[test]
    fn a_run_of_empty_reads_stops_the_board_listening() {
        let repo = fixture("drain-keys-dead-descriptor");
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::for_test();
        let empties = std::iter::repeat_n(None, EMPTY_READS_BEFORE_DEAF as usize + 5);
        let mut stdin = ScriptedStdin::new(empties);
        let mut listening = true;

        let changed = drain_keys(&repo, &pipelines, &mut board, &mut stdin, &mut listening);

        assert!(!listening, "a descriptor that only reads empty is gone");
        assert!(!changed, "nothing was read, so nothing changed");
        assert_eq!(
            stdin.0.len(),
            5,
            "it gave up after {EMPTY_READS_BEFORE_DEAF} rather than reading the rest"
        );
    }

    /// The run has to be unbroken: a key that lands between two empty reads
    /// puts the count back to nothing, so a terminal interrupted now and
    /// again never reads as one that has gone away.
    #[test]
    fn a_key_between_empty_reads_starts_the_count_over() {
        let repo = fixture("drain-keys-interleaved");
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::for_test();
        let mut stdin = ScriptedStdin::new([None, None, Some(b'x'), None, None, Some(b'x')]);
        let mut listening = true;

        drain_keys(&repo, &pipelines, &mut board, &mut stdin, &mut listening);

        assert!(listening, "two in a row, twice over, is not a run of three");
    }

    /// A key that fails reaches the problem log under the board. It used to
    /// be discarded without a word, so a `p` or an `r` that could not be
    /// carried out looked exactly like one that was never read.
    #[test]
    fn a_key_that_fails_is_written_to_the_problem_log() {
        let repo = fixture("drain-keys-failed-key");
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::for_test();
        // `P` reads the whole queue to find what it would abort. A queue
        // directory that is a plain file fails that read, which is the one
        // thing about this test that has to be arranged — every other way a
        // key fails is a race this cannot stage.
        let queue = repo.queue_dir();
        let _ = std::fs::remove_dir_all(&queue);
        std::fs::write(&queue, "not a directory").unwrap();

        let mut stdin = ScriptedStdin::new([Some(b'P')]);
        let mut listening = true;
        drain_keys(&repo, &pipelines, &mut board, &mut stdin, &mut listening);

        let log = std::fs::read_to_string(crate::problem_log::path(&repo))
            .expect("the failed key wrote no problem log at all");
        assert!(log.contains("board key"), "{log}");
        assert!(log.contains("'P'"), "{log}");
    }

    /// A check that returns at once prints its pending row and its done row
    /// back to back, the pending one wiped by the same `\r\x1b[2K` a slow
    /// check would otherwise leave standing alone — acceptance criterion:
    /// each check prints its own line as it returns.
    #[test]
    fn checklist_row_prints_pending_then_wipes_it_for_done() {
        let mut out = Vec::new();
        let value = checklist_row(&mut out, true, "queue read", "reading the queue", || {
            Ok::<_, anyhow::Error>(3)
        })
        .unwrap();
        assert_eq!(value, 3);
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.starts_with(&format!("    {CHECKLIST_PENDING} queue read")),
            "no pending row for the still-running check: {printed:?}"
        );
        assert!(printed.contains("reading the queue"), "{printed:?}");
        assert!(
            printed.contains("\r\x1b[2K"),
            "the pending row must be wiped, not left standing: {printed:?}"
        );
    }

    /// A check that fails prints its pending row and nothing past a bare
    /// newline — the error itself is reported once the board's own
    /// `TermGuard` has already restored the terminal on the way out through
    /// `?`, not raced with a checklist row still open on the same line.
    #[test]
    fn checklist_row_leaves_a_clean_line_on_failure() {
        let mut out = Vec::new();
        let result = checklist_row(
            &mut out,
            true,
            "git identity",
            "checking git identity",
            || Err::<(), _>(anyhow::anyhow!("no identity")),
        );
        assert!(result.is_err());
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.starts_with(&format!("    {CHECKLIST_PENDING} git identity")),
            "{printed:?}"
        );
        assert!(printed.ends_with('\n'), "{printed:?}");
        assert!(!printed.contains("\x1b[2K"), "{printed:?}");
    }

    /// `--plain` (and `args.confirmed`, through the same flag) prints
    /// nothing at all — the checklist is the board's own setup, and
    /// `checklist_row` must be a plain pass-through with `checklist: false`.
    #[test]
    fn checklist_row_prints_nothing_when_the_checklist_is_off() {
        let mut out = Vec::new();
        let value = checklist_row(&mut out, false, "queue read", "reading the queue", || {
            Ok::<_, anyhow::Error>(7)
        })
        .unwrap();
        assert_eq!(value, 7);
        assert!(out.is_empty(), "{out:?}");
    }

    /// `checklist_done`'s own two shapes: a bare done row when there is
    /// nothing to say beside it, and one with a detail column when there is
    /// — the mockup's own two row shapes (`git identity` vs `queue read`).
    #[test]
    fn checklist_done_omits_the_gap_with_no_detail() {
        let mut out = Vec::new();
        checklist_done(&mut out, true, "git identity", "").unwrap();
        checklist_done(&mut out, true, "queue read", "12 tasks").unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(
            printed,
            format!(
                "    {CHECKLIST_DONE} git identity\n    {CHECKLIST_DONE} {}12 tasks\n",
                checklist_label("queue read")
            )
        );
    }

    /// The checklist's own detail for `task routes`: every distinct
    /// `pipeline:` a batch of tasks names, in the order each is first seen —
    /// the mockup's own "impl_ui, impl, release", not an alphabetised list.
    #[test]
    fn task_route_names_lists_each_pipeline_once_in_first_seen_order() {
        let a = crate::task::Task::parse(
            std::path::PathBuf::from("a.md"),
            "---\nid: a\nstage: queued\npipeline: impl_ui\n---\nbody\n",
        )
        .unwrap();
        let b = crate::task::Task::parse(
            std::path::PathBuf::from("b.md"),
            "---\nid: b\nstage: queued\npipeline: impl\n---\nbody\n",
        )
        .unwrap();
        let c = crate::task::Task::parse(
            std::path::PathBuf::from("c.md"),
            "---\nid: c\nstage: queued\npipeline: impl_ui\n---\nbody\n",
        )
        .unwrap();
        let d = crate::task::Task::parse(
            std::path::PathBuf::from("d.md"),
            "---\nid: d\nstage: queued\npipeline: release\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(task_route_names(&[a, b, c, d]), "impl_ui, impl, release");
    }

    /// The 200ms acceptance bound: the board struct's own construction,
    /// followed by the real [`print_checklist_header`] `dispatch` itself
    /// calls, must land well inside it. This cannot exercise the real
    /// `spoolway dispatch` entry point, which needs a live repo and backend;
    /// it instead proves the construction and the print path together add
    /// nothing that could ever cost 200ms on their own, which is the one
    /// thing a future change to either could break. `Board::for_test`'s
    /// guard is inert and calls neither `hide_cursor` nor `raw_mode` — see
    /// its own doc — so the terminal-setup syscalls a real `Board::new`
    /// would make are not, and cannot be, timed here; this bound covers only
    /// the board struct itself and the print path.
    #[test]
    fn the_first_checklist_write_lands_within_200ms() {
        let start = std::time::Instant::now();
        let _board = crate::status::Board::for_test();
        let mut out = Vec::new();
        print_checklist_header(&mut out).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "took {:?}",
            start.elapsed()
        );
    }

    /// A dispatcher already running for this repo must send `spoolway
    /// dispatch` down [`already_running`], never through
    /// [`crate::lock::Lock::acquire`] — a second start reads the run, it
    /// never joins it.
    ///
    /// Proved indirectly rather than by mocking `Lock::acquire`: this process
    /// takes the lock itself first, the same way a real dispatcher would, and
    /// then calls `dispatch` again against the same repo. `Lock::acquire`
    /// refuses a second holder outright — see its own doc comment — so a
    /// second acquire attempt here would surface as this call returning an
    /// error naming "already running", not as a silent takeover. `--plain`
    /// keeps this one call and one exit, with nothing left to interrupt.
    #[test]
    fn a_dispatch_finding_the_lock_held_never_takes_it() {
        let repo = fixture("lock-held-never-taken");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

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
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

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

    /// A task naming no `pipeline:` refuses the whole start, naming the task
    /// and every pipeline defined here — there is no project default left
    /// to route it through instead.
    #[test]
    fn check_task_routes_refuses_a_task_with_no_pipeline() {
        let pipelines = Pipelines::builtin();
        let routeless = crate::task::Task::parse(
            std::path::PathBuf::from("routeless.md"),
            "---\nid: routeless\nstage: queued\n---\nbody\n",
        )
        .unwrap();
        let err = check_task_routes(&pipelines, &[routeless]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("refusing to start: task `routeless` has no `pipeline:`"),
            "{message}"
        );
        assert!(
            message.contains("Set `pipeline:` to one of: bugfix, default."),
            "{message}"
        );
        assert!(message.contains("Nothing was dispatched."), "{message}");
    }

    /// A task naming a pipeline this project does not define refuses the
    /// whole start too, naming both the task and the pipeline it could not
    /// find, alongside the same list of defined choices.
    #[test]
    fn check_task_routes_refuses_a_task_naming_an_unknown_pipeline() {
        let pipelines = Pipelines::builtin();
        let unknown = crate::task::Task::parse(
            std::path::PathBuf::from("unknown.md"),
            "---\nid: unknown\nstage: queued\npipeline: not-a-real-pipeline\n---\nbody\n",
        )
        .unwrap();
        let err = check_task_routes(&pipelines, &[unknown]).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(
                "refusing to start: task `unknown` names a pipeline that does not exist: \
                 `not-a-real-pipeline`"
            ),
            "{message}"
        );
        assert!(
            message.contains("Set `pipeline:` to one of: bugfix, default."),
            "{message}"
        );
    }

    /// A batch where every task names an existing pipeline passes through
    /// untouched — an ordinary explicit route is never refused alongside a
    /// broken one it happens to share a call with.
    #[test]
    fn check_task_routes_passes_every_task_with_an_explicit_existing_pipeline() {
        let pipelines = Pipelines::builtin();
        let a = crate::task::Task::parse(
            std::path::PathBuf::from("a.md"),
            "---\nid: a\nstage: queued\npipeline: default\n---\nbody\n",
        )
        .unwrap();
        let b = crate::task::Task::parse(
            std::path::PathBuf::from("b.md"),
            "---\nid: b\nstage: queued\npipeline: bugfix\n---\nbody\n",
        )
        .unwrap();
        assert!(check_task_routes(&pipelines, &[a, b]).is_ok());
    }

    /// A lock already held exits 4, whether or not it is the first start to
    /// find it that way.
    #[test]
    fn a_lock_already_held_exits_four() {
        let repo = fixture("lock-held-exit");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        let args = DispatchArgs {
            plain: true,
            ..Default::default()
        };
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
            EXIT_ALREADY_RUNNING
        );
    }

    /// A repo whose lock is held, found by a non-`--plain` start, prints the
    /// mockup's own two lines and focuses the pane the lock names — headless
    /// here so this never shells out to a real herdr, and its `focus_pane`
    /// never fails, so this is the ordinary case.
    #[test]
    fn already_running_names_the_pid_and_focuses_the_recorded_pane() {
        let mut repo = fixture("already-running-focuses-pane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, Some("w1:p5")).unwrap();

        let mut out = Vec::new();
        let args = DispatchArgs::default();
        already_running(&repo, &Pipelines::builtin(), 8123, &args, &mut out).unwrap();

        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("a dispatcher is already running for this repo (pid 8123)"),
            "{printed}"
        );
        assert!(printed.contains("→ focusing its pane w1:p5"), "{printed}");
    }

    /// A lock with no pane recorded — an older three-line file, or a run
    /// with nothing to name — prints only the pid line: there is nothing to
    /// focus, so nothing more is said about it.
    #[test]
    fn already_running_says_nothing_about_focus_with_no_pane_recorded() {
        let repo = fixture("already-running-no-pane");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

        let mut out = Vec::new();
        let args = DispatchArgs::default();
        already_running(&repo, &Pipelines::builtin(), 8123, &args, &mut out).unwrap();

        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("a dispatcher is already running for this repo (pid 8123)"),
            "{printed}"
        );
        assert!(!printed.contains("focusing"), "{printed}");
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
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

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
        Pipelines { pipelines }
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

        // Joined a component at a time, the way `check_index_lock` builds it
        // from git's own answer. `join(".git/index.lock")` keeps the embedded
        // `/` verbatim, which renders as a mixed-separator path on Windows and
        // matches nothing in the message under test.
        let lock = repo.root.join(".git").join("index.lock");
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
    /// place at all — one of the two cases `check_backend_checkout` refuses
    /// herdr on, its own doc above.
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

    /// A repo dispatched from a linked worktree — `checkout` a sibling
    /// worktree cut off `root`, `root` the main checkout beside it — paired
    /// with a closure that removes the worktree and the scratch tree
    /// afterwards. Shared by the refusal test below and its `grouped`
    /// counterpart, which differ only in what `Mux` they hand it.
    fn linked_worktree_repo(name: &str) -> (Repo, std::path::PathBuf, impl FnOnce()) {
        let root_dir = crate::scratch::root(&format!("linked-worktree-checkout-{name}"));
        let _ = std::fs::remove_dir_all(&root_dir);
        let main = root_dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        crate::scratch::git_init(&main, &["-b", "main"]);
        std::fs::write(main.join("README"), "hi\n").unwrap();
        crate::repo::run(&main, "git", &["add", "-A"]).unwrap();
        crate::repo::run(&main, "git", &["commit", "-q", "-m", "root"]).unwrap();

        let release = root_dir.join("release");
        crate::repo::run(
            &main,
            "git",
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "release/3",
                release.to_str().unwrap(),
            ],
        )
        .unwrap();

        let repo = Repo {
            root: main.clone(),
            checkout: release.clone(),
            config: Config::default(),
            home: main.join(".home"),
        };
        let main_for_cleanup = main.clone();
        let cleanup = move || {
            crate::repo::run(
                &main_for_cleanup,
                "git",
                &["worktree", "remove", "--force", release.to_str().unwrap()],
            )
            .unwrap();
            std::fs::remove_dir_all(&root_dir).ok();
        };
        (repo, main, cleanup)
    }

    /// A `Mux` standing in for herdr (or anything else), answering only
    /// `name`/`is_available`/`task_owns_workspace` for real — the only three
    /// [`check_backend_checkout`] ever reads — and refusing every other call
    /// outright, so a test that somehow reached one fails loudly rather than
    /// doing something real.
    struct StubMux {
        name: &'static str,
        available: bool,
        /// `Mux::task_owns_workspace`'s own default (`true`) unless a test
        /// sets it otherwise — see `backend_checkout_passes_herdr_under_grouped_mode`,
        /// the one case this matters for `check_backend_checkout`.
        owns_workspace: bool,
        /// `Mux::in_own_pane`'s own default (`true`) unless a test sets it
        /// otherwise — see `dispatcher_visible_refuses_herdr_outside_a_pane`.
        in_own_pane: bool,
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
        fn task_owns_workspace(&self) -> bool {
            self.owns_workspace
        }
        fn in_own_pane(&self) -> bool {
            self.in_own_pane
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
        fn start_lane(
            &self,
            _spec: &crate::mux::LaneSpec<'_>,
            _tick: &mut dyn FnMut(),
        ) -> Result<()> {
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
            name: "headless",
            available: true,
            owns_workspace: true,
            in_own_pane: true,
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
            owns_workspace: true,
            in_own_pane: true,
        };
        assert!(check_backend_checkout(&repo, &mux).unwrap().is_some());
    }

    /// herdr against a bare repository — no main checkout `main_checkout`
    /// can place at all — is refused, naming the path and a way out.
    #[test]
    fn backend_checkout_refuses_herdr_on_a_bare_repository() {
        let repo = bare_repo("herdr-bare");
        let mux = StubMux {
            name: "herdr",
            available: true,
            owns_workspace: true,
            in_own_pane: true,
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

    /// herdr against a dispatcher started inside a linked worktree of its
    /// own project — `repo.checkout` a sibling worktree, `repo.root` the
    /// main checkout beside it — is refused, under `split`. Naming both
    /// checkouts and no backend to switch to is the acceptance criterion in
    /// full: unlike the bare-repository refusal above, there is no other
    /// backend that would fix this, so none is offered.
    ///
    /// `split` specifically: `owns_workspace: true`, the same as the real
    /// `Herdr` under `MuxMode::Split`, whose `Mux::create_workspace` — the
    /// route a task's first cut always takes — hands `open_worktree_workspace`
    /// no fallback at all, a `--cwd` herdr refuses fails that task outright.
    /// See `check_backend_checkout`'s own doc, above it in this module, for
    /// why `split`'s other two routes are still refused the same checkout
    /// here even though each degrades on its own rather than failing
    /// outright, and
    /// `backend_checkout_passes_herdr_on_a_dispatcher_started_in_a_linked_worktree_under_grouped_mode`
    /// for why `grouped` is not refused it at all.
    #[test]
    fn backend_checkout_refuses_herdr_on_a_dispatcher_started_in_a_linked_worktree() {
        let (repo, main, cleanup) = linked_worktree_repo("split");
        let mux = StubMux {
            name: "herdr",
            available: true,
            owns_workspace: true,
            in_own_pane: true,
        };

        let bare = format!("{:#}", check_backend_checkout(&repo, &mux).unwrap_err());
        assert!(
            bare.contains(&repo.checkout.display().to_string()),
            "{bare}"
        );
        assert!(bare.contains(&main.display().to_string()), "{bare}");

        let refused = format!(
            "{:#}",
            refuse(check_backend_checkout(&repo, &mux)).unwrap_err()
        );
        assert!(
            refused.contains(&main.display().to_string()),
            "the fix names the checkout to run from instead: {refused}"
        );
        assert!(
            !refused.contains("dispatch.backend"),
            "no backend switch is offered — herdr is refused this checkout, not this project: \
             {refused}"
        );

        cleanup();
    }

    /// The same dispatcher-in-a-linked-worktree shape as the `split` test
    /// above, but under `grouped` — `owns_workspace: false`, matching
    /// `Herdr::task_owns_workspace` under `MuxMode::Grouped` — passes.
    ///
    /// `grouped`'s own per-task route is `project_tab`, in `src/dispatch.rs`:
    /// every branch of `start_one`'s dispatch match — a borrowed checkout, a
    /// freshly cut one, a healed stale pane — takes the `task_owns_workspace
    /// == false` arm into `project_tab` unconditionally, which opens the
    /// shared dispatch workspace on `dispatch_home()` and the task's own tab
    /// on the task's checkout directly, through `Mux::dispatch_workspace`
    /// and `Mux::open_tab`. Neither reads `Herdr::anchor` or calls
    /// `open_worktree_workspace` at all, so `grouped`'s ordinary dispatch
    /// loop never hands herdr this checkout as `--cwd` in the first place —
    /// there is nothing here for a linked worktree to break, and this
    /// non-goal is preserved by never refusing it up front.
    #[test]
    fn backend_checkout_passes_herdr_on_a_dispatcher_started_in_a_linked_worktree_under_grouped_mode()
     {
        let (repo, _main, cleanup) = linked_worktree_repo("grouped");
        let mux = StubMux {
            name: "herdr",
            available: true,
            owns_workspace: false,
            in_own_pane: true,
        };

        assert!(
            check_backend_checkout(&repo, &mux).unwrap().is_some(),
            "grouped dispatch never hands herdr this checkout as --cwd, so it is not this \
             refusal's to make"
        );

        cleanup();
    }

    /// A herdr run with no pane to draw in is refused, naming the way in —
    /// `herdr` and `spoolway dispatch` — with no exemption.
    #[test]
    fn dispatcher_visible_refuses_herdr_outside_a_pane() {
        let mux = StubMux {
            name: "herdr",
            available: true,
            owns_workspace: true,
            in_own_pane: false,
        };
        let err = format!("{:#}", check_dispatcher_visible(&mux).unwrap_err());
        assert!(err.contains("has to be visible"), "{err}");
        assert!(err.contains("herdr"), "{err}");
        assert!(err.contains("spoolway dispatch"), "{err}");
    }

    /// A herdr run that is in a pane passes straight through.
    #[test]
    fn dispatcher_visible_passes_herdr_in_a_pane() {
        let mux = StubMux {
            name: "herdr",
            available: true,
            owns_workspace: true,
            in_own_pane: true,
        };
        assert!(check_dispatcher_visible(&mux).is_ok());
    }

    /// `backend = headless` with no harness marker is refused, naming it as
    /// the test backend and offering the way back to herdr.
    ///
    /// Read through [`crate::platform::env_var`] rather than `std::env::var`
    /// so the marker can be set per-thread below without touching the real
    /// process environment every other test shares.
    #[test]
    fn dispatcher_visible_refuses_headless_with_no_marker() {
        let mux = StubMux {
            name: "headless",
            available: true,
            owns_workspace: true,
            in_own_pane: false,
        };
        let err = format!("{:#}", check_dispatcher_visible(&mux).unwrap_err());
        assert!(err.contains("test backend"), "{err}");
        assert!(
            err.contains("spoolway config set dispatch.backend herdr"),
            "{err}"
        );
    }

    /// The same run passes once the end-to-end harness's own marker is set —
    /// `in_own_pane: false` alongside it, to prove the marker is what gates
    /// this backend rather than the pane check meant for herdr.
    #[test]
    fn dispatcher_visible_passes_headless_with_the_marker_set() {
        let mux = StubMux {
            name: "headless",
            available: true,
            owns_workspace: true,
            in_own_pane: false,
        };
        let result =
            crate::platform::test_env::with_env(crate::headless::TEST_BACKEND_ENV, "1", || {
                check_dispatcher_visible(&mux)
            });
        assert!(result.is_ok());
    }

    /// A project with no layer at all is nothing to ask about — the gate
    /// must return straight through to an ordinary run without ever
    /// consulting `ask::interactive()`, so this passes deterministically
    /// whether or not the test process happens to have a real terminal.
    #[test]
    fn overrides_gate_with_no_layer_proceeds_without_asking() {
        let repo = fixture("overrides-gate-no-layer");
        assert!(overrides_gate(&repo, true).unwrap());
    }

    /// A fixture with a real layer on it, forked the same way `spoolway
    /// pipeline override` writes one — for every `overrides_gate_with` case
    /// below, which needs an actual `OverrideRow` to ask about.
    fn fixture_with_layer(name: &str) -> Repo {
        let repo = fixture(name);
        std::fs::create_dir_all(repo.overrides_dir().join("pipelines")).unwrap();
        std::fs::write(
            repo.overrides_dir().join("pipelines/default.yml"),
            "steps:\n  implement:\n    model: fake-opus\n",
        )
        .unwrap();
        repo
    }

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    /// Acceptance criterion 5, exercised as a real unit test rather than only
    /// through the e2e suite: with a layer present and nobody there to
    /// answer, the notice is printed and the run proceeds without ever
    /// reading a key — `input` is left empty on purpose, so a `read_key` call
    /// here would hang the test rather than fail it.
    #[test]
    fn overrides_gate_with_a_layer_and_no_tty_proceeds_having_printed() {
        let repo = fixture_with_layer("overrides-gate-no-tty");
        let mut input = keys("");
        let mut out = Vec::new();
        let proceed = overrides_gate_with(
            &repo,
            false,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        assert!(proceed);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("overrides are active"), "{printed}");
        assert!(printed.contains("pipelines/default.yml"), "{printed}");
    }

    /// `enter` starts an otherwise ordinary run — no acknowledgement is
    /// written, since the mockup reserves that for `x` alone.
    #[test]
    fn overrides_gate_enter_proceeds_without_acknowledging() {
        let repo = fixture_with_layer("overrides-gate-enter");
        let mut input = keys("\r");
        let mut out = Vec::new();
        assert!(
            overrides_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
        assert!(
            crate::overrides::ack_needed(
                &repo.home,
                &crate::version::layer_fingerprint(&repo).unwrap()
            ),
            "enter must not count as an acknowledgement"
        );
    }

    /// `esc` is the one path that must reach the caller as `false`, before
    /// `dispatch` ever takes the lock — and it writes no acknowledgement
    /// either.
    #[test]
    fn overrides_gate_esc_declines() {
        let repo = fixture_with_layer("overrides-gate-esc");
        let mut input = keys("\x1b");
        let mut out = Vec::new();
        assert!(
            !overrides_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
        assert!(crate::overrides::ack_needed(
            &repo.home,
            &crate::version::layer_fingerprint(&repo).unwrap()
        ));
    }

    /// `x` starts the run and records the layer's own fingerprint — and once
    /// recorded, an unchanged layer never asks again, exactly as "don't ask
    /// again until this changes" promises: the second call needs no input at
    /// all, or it would hang rather than pass.
    #[test]
    fn overrides_gate_x_acknowledges_and_is_not_asked_again() {
        let repo = fixture_with_layer("overrides-gate-x");
        let fingerprint = crate::version::layer_fingerprint(&repo).unwrap();

        let mut input = keys("x");
        let mut out = Vec::new();
        assert!(
            overrides_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
        assert!(!crate::overrides::ack_needed(&repo.home, &fingerprint));

        let mut no_input = keys("");
        let mut out = Vec::new();
        assert!(
            overrides_gate_with(
                &repo,
                true,
                &mut no_input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
        assert!(
            out.is_empty(),
            "an unchanged, acknowledged layer must say nothing at all: {out:?}"
        );
    }

    /// The mockup's own column: a pipeline or config patch counts its keys,
    /// singular or plural, and a whole-file prompt override just says so.
    #[test]
    fn overrides_gate_kind_counts_keys_and_names_a_whole_file() {
        let two_keys = OverrideRow {
            target: "pipelines/impl.yml".into(),
            kind: "patch",
            overrides: "implement.model, test.timeout".into(),
        };
        assert_eq!(overrides_gate_kind(&two_keys), "2 keys");

        let one_key = OverrideRow {
            target: "config.toml".into(),
            kind: "patch",
            overrides: "agents.claude.concurrency".into(),
        };
        assert_eq!(overrides_gate_kind(&one_key), "1 key");

        let prompt = OverrideRow {
            target: "prompts/reviewer".into(),
            kind: "whole file",
            overrides: "—".into(),
        };
        assert_eq!(overrides_gate_kind(&prompt), "whole file");
    }

    /// A task for [`overview_lines`]'s own tests — `extra` carries whatever
    /// of `group:`, `pipeline:`, `base:` and `stage:` a case wants; `stage:`
    /// has no default here the way [`crate::pipeline::QUEUED`] gives a real
    /// queued task one, since a case testing the STEP column has to be free
    /// to name something other than `queued`.
    fn overview_task(id: &str, extra: &str) -> Task {
        crate::task::Task::parse(
            std::path::PathBuf::from(format!("{id}.md")),
            &format!("---\nid: {id}\n{extra}---\nbody\n"),
        )
        .unwrap()
    }

    /// Groups sort by name, alphabetically — `alpha` before `zebra` — and a
    /// task naming no `group:` is a group of one, keyed by its own id, the
    /// same reading `crate::status::Row::group` gives it.
    #[test]
    fn overview_lines_groups_by_group_sorted_by_name_falling_back_to_the_task_id() {
        let tasks = [
            overview_task(
                "a",
                "group: zebra\npipeline: default\nstage: queued\nbase: main\n",
            ),
            overview_task(
                "b",
                "group: alpha\npipeline: default\nstage: queued\nbase: main\n",
            ),
            overview_task("c", "pipeline: default\nstage: queued\nbase: main\n"),
        ];
        let lines = overview_lines(&tasks, None);
        assert_eq!(lines[0], "queued  3 groups · 3 tasks", "{lines:?}");

        let alpha = lines.iter().position(|l| l == "alpha").unwrap();
        let c = lines.iter().position(|l| l == "c").unwrap();
        let zebra = lines.iter().position(|l| l == "zebra").unwrap();
        assert!(
            alpha < c && c < zebra,
            "groups must sort alphabetically, `c` (task `c`'s own group of one) included: \
             {lines:?}"
        );
    }

    /// A task naming no `pipeline:` or no `base:` — a legacy or hand-edited
    /// document, since `queue add` always stamps both — draws an em dash in
    /// that cell rather than an empty one a person could mistake for a
    /// column that slipped out of alignment.
    #[test]
    fn overview_lines_draws_an_em_dash_for_a_missing_pipeline_or_base() {
        let tasks = [overview_task("solo", "group: solo-group\nstage: queued\n")];
        let lines = overview_lines(&tasks, None);
        // Skip the group header line itself ("solo-group") — the row is the
        // one starting with two spaces, indented under it.
        let row = lines.iter().find(|l| l.starts_with("  solo")).unwrap();
        assert_eq!(
            row,
            &format!(
                "  {}{}{}{}",
                overview_cell("solo", OVERVIEW_NAME_W),
                overview_cell("—", OVERVIEW_PIPELINE_W),
                overview_cell("queued", OVERVIEW_STEP_W),
                "—"
            )
        );
    }

    /// Review finding 1's own repro: a task id, pipeline, step or base too
    /// long for its column is cut with an ellipsis rather than pushing every
    /// column after it — and the row overall never runs past 80 columns.
    #[test]
    fn overview_lines_cuts_a_long_id_pipeline_step_or_base_rather_than_overflowing() {
        let tasks = [overview_task(
            "a-task-id-much-longer-than-the-twenty-column-budget",
            "group: over\n\
             pipeline: a-pipeline-name-far-too-long-for-its-own-column\n\
             stage: an-implausibly-long-step-name-for-its-column\n\
             base: feature/rework-the-dispatcher-lock-handling-end-to-end\n",
        )];
        let lines = overview_lines(&tasks, None);
        let row = lines
            .iter()
            .find(|l| l.trim_start().starts_with("a-task-id"))
            .unwrap();
        assert!(
            row.chars().count() <= 80,
            "a row must never run past 80 columns: {} ({})",
            row.chars().count(),
            row
        );
        assert!(row.contains('…'), "{row:?}");
        // Review finding 1's second half: a cell clipped to its full column
        // width ran edge to edge into the next column with no separating
        // space, unlike the mockup's own gapped columns. The last character
        // of each of the first three (fixed-width) cells must be the space
        // `overview_cell` now reserves out of its own budget.
        let chars: Vec<char> = row.chars().collect();
        for boundary in [
            2 + OVERVIEW_NAME_W - 1,
            2 + OVERVIEW_NAME_W + OVERVIEW_PIPELINE_W - 1,
            2 + OVERVIEW_NAME_W + OVERVIEW_PIPELINE_W + OVERVIEW_STEP_W - 1,
        ] {
            assert_eq!(
                chars[boundary], ' ',
                "column must end in a gap, not run into the next one: {row:?}"
            );
        }
    }

    /// A project with nothing queued at all still draws — an empty overview
    /// rather than a screen with nothing between the header and the footer
    /// line a person could mistake for a stalled draw.
    #[test]
    fn overview_lines_with_no_tasks_still_draws_the_header_and_footer() {
        let lines = overview_lines(&[], None);
        assert_eq!(lines[0], "queued  0 groups · 0 tasks", "{lines:?}");
        assert_eq!(
            lines.last(),
            Some(&"[enter] start a dispatcher   [esc] back".to_string()),
            "{lines:?}"
        );
    }

    /// `held_pid: Some` — the `focus-live-run` mockup — swaps the footer for
    /// one describing what `enter` now does and inserts the pid line right
    /// under the header, ahead of the blank line separating it from the
    /// table.
    #[test]
    fn overview_lines_with_a_held_pid_draws_the_notice_and_swaps_the_footer() {
        let lines = overview_lines(&[], Some(250));
        assert_eq!(lines[0], "queued  0 groups · 0 tasks", "{lines:?}");
        assert_eq!(
            lines[1], "a dispatcher is already running (pid 250) — it takes these on its next pass",
            "{lines:?}"
        );
        assert_eq!(lines[2], "", "{lines:?}");
        assert_eq!(
            lines.last(),
            Some(&"[enter] go to the dispatcher   [esc] back".to_string()),
            "{lines:?}"
        );
    }

    /// Never drawn with nobody there to answer: unlike the overrides
    /// notice, this has no record to leave in a log nobody is watching, so
    /// a non-interactive dispatch prints nothing about it at all and never
    /// reads a key.
    #[test]
    fn overview_gate_with_non_interactive_is_never_drawn() {
        let repo = fixture("overview-gate-non-interactive");
        let mut input = keys("");
        let mut out = Vec::new();
        let proceed = overview_gate_with(
            &repo,
            false,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "{out:?}");
    }

    /// `enter` proceeds to the overrides gate — the whole of what the
    /// overview's own `[enter]` promises.
    #[test]
    fn overview_gate_with_enter_proceeds() {
        let repo = fixture("overview-gate-enter");
        let mut input = keys("\r");
        let mut out = Vec::new();
        assert!(
            overview_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert)
            )
            .unwrap()
        );
        let drawn = String::from_utf8(out).unwrap();
        assert!(drawn.contains("queued  0 groups · 0 tasks"), "{drawn}");
    }

    /// `esc` is the one path that must reach the caller as `false`, the
    /// same as `overrides_gate_with`'s own.
    #[test]
    fn overview_gate_with_esc_declines() {
        let repo = fixture("overview-gate-esc");
        let mut input = keys("\x1b");
        let mut out = Vec::new();
        assert!(
            !overview_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert)
            )
            .unwrap()
        );
    }

    /// The tty going away mid-question declines here, unlike
    /// `overrides_gate_with`'s own copy of the same read, which proceeds —
    /// see that function's own doc comment on why the two differ: this
    /// screen is also reached through `commands::queue::confirm_start`,
    /// where an exhausted pipe is the ordinary way a script ends the queue
    /// screen, not a real terminal dying mid-answer.
    #[test]
    fn overview_gate_with_none_declines() {
        let repo = fixture("overview-gate-none");
        let mut input = keys("");
        let mut out = Vec::new();
        assert!(
            !overview_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert)
            )
            .unwrap()
        );
    }

    /// `enter` on the held-lock draw — task `focus-live-run`'s own mockup —
    /// says `true`: there is somewhere to go now, not a run to start, but
    /// the gate's shape is the same "proceed or not" either way.
    #[test]
    fn dispatcher_running_gate_with_enter_proceeds() {
        let repo = fixture("dispatcher-running-gate-enter");
        let mut input = keys("\r");
        let mut out = Vec::new();
        assert!(dispatcher_running_gate_with(&repo, 250, &mut input, &mut out).unwrap());
        let drawn = String::from_utf8(out).unwrap();
        assert!(
            drawn.contains(
                "a dispatcher is already running (pid 250) — it takes these on its next pass"
            ),
            "{drawn}"
        );
        assert!(
            drawn.contains("[enter] go to the dispatcher   [esc] back"),
            "{drawn}"
        );
    }

    /// `esc` declines, back to browsing — the same reading every other gate
    /// in this file gives it.
    #[test]
    fn dispatcher_running_gate_with_esc_declines() {
        let repo = fixture("dispatcher-running-gate-esc");
        let mut input = keys("\x1b");
        let mut out = Vec::new();
        assert!(!dispatcher_running_gate_with(&repo, 250, &mut input, &mut out).unwrap());
    }

    /// The tty going away mid-question declines here too — the same
    /// conservative reading [`overview_gate_with_none_declines`] gives its
    /// own copy of this case, since this screen is likewise reached only
    /// through the queue screen, where an exhausted pipe is the ordinary
    /// way a script ends it.
    #[test]
    fn dispatcher_running_gate_with_none_declines() {
        let repo = fixture("dispatcher-running-gate-none");
        let mut input = keys("");
        let mut out = Vec::new();
        assert!(!dispatcher_running_gate_with(&repo, 250, &mut input, &mut out).unwrap());
    }

    /// The mockup's own three headings, in order, each skipped when its
    /// section is empty — [`print_warnings_notice`] and
    /// [`warnings_gate_with`]'s own fingerprint both build on this.
    #[test]
    fn warnings_lines_draws_only_the_sections_with_something_in_them() {
        assert!(
            warnings_lines(&[], &[], &[]).is_empty(),
            "nothing to say is nothing drawn"
        );

        let settings_only = warnings_lines(&["a setting".to_string()], &[], &[]);
        assert_eq!(settings_only[0], "settings");
        assert!(!settings_only.contains(&"files".to_string()));
        assert!(!settings_only.contains(&"problems".to_string()));

        let all_three = warnings_lines(
            &["a setting".to_string()],
            &["a file".to_string()],
            &["a problem".to_string()],
        );
        let heading_order: Vec<&str> = all_three
            .iter()
            .filter(|l| ["settings", "files", "problems"].contains(&l.as_str()))
            .map(String::as_str)
            .collect();
        assert_eq!(heading_order, vec!["settings", "files", "problems"]);
    }

    /// Two items under the same heading are a single line apart, not two —
    /// the blank line this screen used to leave between them is gone, so a
    /// scan of names has nothing to skip over.
    #[test]
    fn warnings_lines_puts_no_blank_line_between_items() {
        let lines = warnings_lines(
            &["first setting".to_string(), "second setting".to_string()],
            &[],
            &[],
        );
        assert_eq!(
            lines,
            vec![
                "settings".to_string(),
                "  first setting".to_string(),
                "  second setting".to_string(),
            ]
        );
    }

    /// A section's text wraps at 80 columns, the first line at `indent` and
    /// every line after it two columns further in — the mockup's own hang,
    /// the only thing telling a wrapped continuation apart from the next
    /// item now that nothing sits between them.
    #[test]
    fn warnings_lines_wraps_a_long_line_with_a_two_column_hang() {
        let long = "word ".repeat(30);
        let lines = warnings_lines(&[long], &[], &[]);
        assert!(lines[1].starts_with("  word"), "{lines:?}");
        for line in &lines[2..] {
            if line.is_empty() {
                continue;
            }
            assert!(line.starts_with("    "), "{lines:?}");
            assert!(line.chars().count() <= 80, "{lines:?}");
        }
    }

    /// A fresh project has never been initialised, so `doctor_sync`'s own
    /// notes fire and land under "files" — enough to prove this screen has
    /// something to say without a tty, and prints it without ever reading a
    /// key (`input` is left empty; a `read_key` call here would hang the
    /// test).
    #[test]
    fn warnings_gate_with_no_tty_prints_and_proceeds() {
        let repo = fixture("warnings-gate-no-tty");
        let pipelines = Pipelines::builtin();
        let mut input = keys("");
        let mut out = Vec::new();
        let proceed = warnings_gate_with(
            &repo,
            &pipelines,
            false,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        assert!(proceed);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("before this run starts"), "{printed}");
        assert!(printed.contains("files"), "{printed}");
    }

    /// `enter` starts the run without writing an acknowledgement — the
    /// mockup reserves that for `x` alone, the same rule
    /// `overrides_gate_with` follows. `unattended.enabled` with no ceiling
    /// set is what guarantees this screen has a setting worth reading,
    /// rather than skipping itself for having nothing to say.
    #[test]
    fn warnings_gate_enter_proceeds_without_acknowledging() {
        let mut repo = fixture("warnings-gate-enter");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        let mut input = keys("\r");
        let mut out = Vec::new();
        assert!(
            warnings_gate_with(
                &repo,
                &pipelines,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
    }

    /// `esc` is the one path that must reach the caller as `false`, before
    /// anything below it in `dispatch` ever spawns a lane.
    #[test]
    fn warnings_gate_esc_declines() {
        let mut repo = fixture("warnings-gate-esc");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        let mut input = keys("\x1b");
        let mut out = Vec::new();
        assert!(
            !warnings_gate_with(
                &repo,
                &pipelines,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
    }

    /// `x` starts the run and records a fingerprint of the rendered lines —
    /// and once recorded, the same unchanged lines never ask again: the
    /// second call needs no input at all, or it would hang rather than pass,
    /// and it draws nothing.
    #[test]
    fn warnings_gate_x_acknowledges_and_is_not_asked_again() {
        let mut repo = fixture("warnings-gate-x");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();

        let mut input = keys("x");
        let mut out = Vec::new();
        assert!(
            warnings_gate_with(
                &repo,
                &pipelines,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );

        let mut no_input = keys("");
        let mut out = Vec::new();
        assert!(
            warnings_gate_with(
                &repo,
                &pipelines,
                true,
                &mut no_input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap()
        );
        assert!(
            out.is_empty(),
            "unchanged, acknowledged lines must say nothing at all: {out:?}"
        );
    }

    /// Review finding 2: the footer sits flush left, in the same column as
    /// the headings above it — unlike the overrides gate's own footer, whose
    /// indent matches a body that is itself indented two columns, this
    /// screen's headings are not.
    #[test]
    fn warnings_gate_footer_is_flush_left() {
        let mut repo = fixture("warnings-gate-footer");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        let mut input = keys("\x1b");
        let mut out = Vec::new();
        warnings_gate_with(
            &repo,
            &pipelines,
            true,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("\n[enter] start the run"),
            "footer must not be indented: {printed:?}"
        );
    }

    /// `workspace_open_notice_with`'s own case: no tty, so the failure is
    /// printed once, on record, and nothing here blocks on a key nobody can
    /// answer — `input` is left empty, or a `read_key` call would hang the
    /// test.
    #[test]
    fn workspace_open_notice_with_no_tty_prints_and_returns() {
        let mut input = keys("");
        let mut out = Vec::new();
        workspace_open_notice_with(
            "could not open this run's own workspace: nope",
            false,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("problems"), "{printed}");
        assert!(printed.contains("nope"), "{printed}");
    }

    /// `[enter]` is the only key this reads — there is no earlier screen
    /// left to decline back to by the time this notice can show, so unlike
    /// `warnings_gate_with` there is no `esc` branch to exercise here at
    /// all.
    #[test]
    fn workspace_open_notice_with_enter_dismisses() {
        let mut input = keys("\r");
        let mut out = Vec::new();
        workspace_open_notice_with(
            "could not open this run's own workspace: nope",
            true,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("[enter] continue"), "{printed}");
    }

    /// The tty going away mid-question dismisses rather than hangs — the
    /// same reasoning `warnings_gate_with`'s own `None` branch gives, and for
    /// the same reason: nothing here may wait forever for an answer that can
    /// no longer come.
    #[test]
    fn workspace_open_notice_with_none_dismisses() {
        let mut input = keys("");
        let mut out = Vec::new();
        workspace_open_notice_with(
            "could not open this run's own workspace: nope",
            true,
            &mut input,
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
    }
}
