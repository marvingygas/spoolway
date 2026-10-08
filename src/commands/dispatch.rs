//! `spoolway dispatch`: the run loop around [`crate::dispatch`], and stopping one.

use anyhow::anyhow;

use super::*;
use crate::dispatch::skip_wait;
use crate::screen::PollableRead;

/// An empty queue is an ordinary ending: nothing was there to dispatch, and
/// running again once something is queued is exactly what a person or a
/// script should do next. Only when no job is enabled — an enabled job keeps
/// the run resident on an empty queue instead, so it is alive when the
/// job's window comes round — and never for a run bare `spoolway` started
/// (`--from-screen`), which waits on an empty queue until the screen stops
/// it.
pub const EXIT_EMPTY_QUEUE: i32 = 3;

/// Another dispatcher, or bare `spoolway`'s own screen, already owns this
/// project. Ordinary too — the caller's own next pass, or the one already
/// running, will pick up whatever it queued — but distinct from an empty
/// queue, since a caller restarting a dispatcher forever needs to be able
/// to tell the two apart.
pub const EXIT_ALREADY_RUNNING: i32 = 4;

/// The one line either refusal prints, every time, with no count or pid
/// behind it — a person reading two different starts a minute apart must
/// see the same words, not a message that happens to differ because one
/// was refused by the screen's lock and the other by the dispatcher's own.
///
/// `pub(crate)` so bare `spoolway`'s own refusal, in
/// [`crate::screen::shell::run`], prints this exact constant rather than a
/// second copy of the words that could drift from it.
pub(crate) const ALREADY_RUNNING: &str = "Dispatcher already running";

/// Whether this project already has a `spoolway` in it — its screen, or a
/// dispatcher, whichever is up.
///
/// `from_screen` is `args.screen` — [`DispatchArgs::screen`], the hidden
/// `--from-screen` bare `spoolway`'s dispatch tab starts its own child
/// with. That child skips the screen's own lock: it is the screen's, not a
/// second `spoolway`, and checking it here would refuse the child against
/// its own parent. It still checks the dispatcher's lock, the same as every
/// other caller, since another dispatcher already running is exactly the
/// case that lock exists to catch.
pub(crate) fn already_running(repo: &Repo, from_screen: bool) -> Result<bool> {
    if !from_screen && crate::lock::Lock::holder(&repo.screen_lock_file())?.is_some() {
        return Ok(true);
    }
    Ok(crate::lock::Lock::holder(&repo.lock_file())?.is_some())
}

/// Run the pipeline: one pass, or a loop on the dispatcher's fixed poll rate.
///
/// Returns the code the process should exit with, rather than `()`, so a
/// caller restarting this in a tight loop against a repo that cannot run
/// can tell an empty queue (3), a lock already held (4), and a run that
/// dispatched and stopped on its own (0) apart. Every genuine error is
/// still `Err` — see `crate::main` for where each of these becomes a
/// process exit code.
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

    // One dispatcher, or one screen, serves the whole repo, whichever
    // worktree it was started in: the queue belongs to the main checkout,
    // and a task carries the branch it was queued on. So a dispatcher that
    // is already running will pick up what was just queued from another
    // plan's worktree on its next pass — there is nothing to start, and
    // saying so is not a failure. The same line every time, with no count
    // behind it: see [`already_running`].
    if already_running(repo, args.screen)? {
        // To stderr under the screen, which shows it as the reason this
        // start did not happen; stdout otherwise, where it always was.
        match args.screen {
            true => eprintln!("{ALREADY_RUNNING}"),
            false => println!("{ALREADY_RUNNING}"),
        }
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
    let live_tasks = repo.tasks()?;
    // A run the screen started is the exception: it waits on an empty queue
    // for whatever the queue tab sends next, and the board says `nothing
    // queued` meanwhile — the screen, not the queue, decides when it ends.
    if live_tasks.is_empty() && !args.screen {
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
    check_task_routes(pipelines, &live_tasks)?;
    check_task_bases(repo, &live_tasks)?;

    let mux = crate::mux::backend(repo)?;
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

    // There is one way to start a run and it is visible: refused here, in the
    // same early group as the three checks above, so a run begun in a
    // backgrounded shell or a `backend = headless` config edit outside the
    // harness is stopped before it takes the lock or writes anywhere, not
    // found and killed by hand once it is already spending. No exemption —
    // not an environment override, not a per-platform carve-out.
    check_dispatcher_visible(mux.as_ref())?;

    // The last two things a person sees before anything is spawned or
    // written: the overrides gate, since a layer changes what runs without
    // `git status` ever hinting that it is on; and doctor's own cheap
    // findings, read as a warning rather than discovered mid-run. Both run
    // here, before the lock is taken and the mode is written into it, so
    // `esc` off either can back out having done nothing at all.
    // `args.screen` skips both: bare `spoolway`'s dispatch tab already asked
    // them as popups of its own, off [`overrides_popup`] and
    // [`warnings_popup`], before it started this run.
    if !args.screen {
        if !overrides_gate(repo)? {
            return Ok(0);
        }
        if !warnings_gate(repo, pipelines)? {
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

    // Losing the race for the lock is not an error. `already_running` above looked
    // first, but two starts can both pass that look before either has
    // written the file, and the loser must read exactly as one that looked a
    // moment later.
    let _lock = match crate::lock::Lock::acquire(&repo.lock_file(), unattended, pane_id.as_deref())
    {
        Ok(lock) => lock,
        Err(err) if err.downcast_ref::<crate::lock::HeldBy>().is_some() => {
            match args.screen {
                true => eprintln!("{ALREADY_RUNNING}"),
                false => println!("{ALREADY_RUNNING}"),
            }
            return Ok(EXIT_ALREADY_RUNNING);
        }
        Err(err) => return Err(err),
    };

    // What this run loaded, written down the moment the lock is ours, so a
    // lane's `report` — and `resume` and `queue add` — routes on the same
    // graph this dispatcher does rather than on files edited since. See
    // `crate::pipeline_snapshot`. Before any lane exists, so no lane of
    // this run can report ahead of it.
    crate::pipeline_snapshot::write(repo, pipelines).context("refusing to start")?;

    // Note this project once per run, so `spoolway eval --all` can find
    // its ledger later. A project that is dispatched in is a project that spends.
    crate::usage::registry::register(&repo.root);

    // Trimmed once, here, rather than on every append below — see
    // `crate::problem_log::open`. A run that never hits a problem never
    // touches this file at all.
    crate::problem_log::open(repo);

    let interval = crate::dispatch::PROBE_INTERVAL;

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

    // Whatever the last stop interrupted — the dispatch tab's stop popup,
    // `i` — goes back to work before the first pass, so that pass launches
    // it. Here rather than in the tab, so a start typed at a terminal
    // resumes the same tasks. See `crate::status::resume_stop_parked`.
    for problem in crate::status::resume_stop_parked(repo, pipelines)? {
        crate::problem_log::append(repo, &problem);
        println!("  ! {problem}");
    }

    // Wakes the wait below the moment a finished background command lands,
    // instead of it being found up to `interval` later — see
    // `crate::screen::DirWatch`. `None` on a target with nothing to watch
    // with, or if opening the watch failed for some other reason; either way
    // the wait below falls back to the plain interval alone, exactly as it
    // behaved before this task.
    let watch = crate::screen::open_dir_watch(&repo.commands_dir());

    loop {
        let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux.as_ref());

        let mut spent_out = None;
        // Whether this pass moved a task and so has more ready to try at
        // once — see the wait below, and `dispatch::skip_wait`. A pass that
        // errored outright never sets this: a transient failure should cost
        // one wait, not be retried with no pause at all.
        let mut worked = false;

        match dispatcher.pass(&mut || {}) {
            Ok(report) => {
                worked = skip_wait(&report, consecutive_working);
                // The ceiling drains rather than kills: the run is over, but not
                // until whatever is mid-turn has had its chance to report. A
                // lane torn down halfway spent its tokens and produced nothing.
                if let Some(ceiling) = &report.ceiling
                    && !report.lanes_live
                {
                    spent_out = Some(ceiling.clone());
                }
                for problem in &report.problems {
                    crate::problem_log::append(repo, problem);
                }
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
            // A failed pass must not kill the loop: a transient herdr hiccup or
            // a half-written task file should cost one pass, not the run.
            Err(err) => {
                let failure = format!("pass failed: {err:#}");
                crate::problem_log::append(repo, &failure);
                println!("  ! {failure}");
            }
        }

        // Spent, and nothing left running to spend more. The queue keeps its
        // place: every task is where its last lane left it, and the next run
        // picks them up from exactly there.
        if let Some(ceiling) = spent_out {
            stop(repo, pipelines, mux.as_ref())?;
            // The screen shows stderr as the reason its dispatcher stopped,
            // and prints nothing of stdout at all.
            if args.screen {
                eprintln!("{}", screen_stop_reason(&ceiling));
                return Ok(0);
            }
            println!("  {}", ceiling.note);
            println!("  spoolway dispatch    # picks the queue back up where it stands");
            return Ok(0);
        }

        // A task file leaves the queue only when its task reaches `done`,
        // so an empty queue means every task is finished — including any queued
        // from another plan's worktree while this loop was running, since they
        // all arrive in this one queue. A blocked or paused task stays in the
        // queue, and so does the loop: those are waiting on a person, not done.
        // Asked for while the last pass was running. Unwound here rather than
        // in the handler, which may do nothing but set the flag.
        if crate::platform::stop::asked() {
            stop(repo, pipelines, mux.as_ref())?;
            println!("  stopped.");
            return Ok(0);
        }

        match repo.tasks() {
            Ok(tasks) if tasks.is_empty() => {
                // Not for a run the screen started — see the same carve-out
                // on the queue read before the loop.
                if crate::jobs::enabled_count(repo) == 0 && !args.screen {
                    stop(repo, pipelines, mux.as_ref())?;
                    println!("  queue is empty — every task is done. Stopping.");
                    return Ok(0);
                }
                // A job keeps the run resident. Say so once per spell of
                // empty queue, not every pass, then loop on into the wait.
                if !idle_announced {
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
            continue;
        }
        consecutive_working = 0;

        // The wait between passes: the floor alone, unless a finished
        // background command wakes it early — see
        // `crate::screen::open_dir_watch`'s own doc.
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
                // The floor alone, in the same slices as before — see
                // `crate::screen::open_dir_watch`'s own doc — so a stop asked
                // for mid-wait is still noticed within a second rather than a
                // whole interval later.
                std::thread::sleep(crate::status::POLL.min(left));
                continue;
            };
            let ready = crate::screen::poll_ready(&[watch.fd()], left);
            if ready[0] && watch.drain() {
                break;
            }
        }
    }
}

/// Why a dispatcher bare `spoolway` started stopped on a spend ceiling, as
/// its dispatch tab's popup shows it: the key and its figures, then where
/// that leaves the queue.
fn screen_stop_reason(ceiling: &crate::dispatch::Ceiling) -> String {
    format!(
        "{}\nEvery task is where its last lane left it.",
        ceiling.reached
    )
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

/// The standing consent gate for a patch layer (see [`crate::overrides`]):
/// `Ok(true)` to go on and start the run, `Ok(false)` only for `esc`, the one
/// path that must reach the caller before `Lock::acquire` runs at all.
///
/// A thin wrapper over [`overrides_gate_with`], which does the real work
/// against an injected reader, writer and terminal guard — this is the only
/// thing that touches the process's real stdio. `TermGuard::screen` is
/// handed over as a factory rather than constructed here: raw mode is a
/// presentation detail for the one branch that actually blocks on a
/// keypress, and building it eagerly printed `hide_cursor`'s escape into
/// every single dispatch, layered or not, tty or not (finding: `hide_cursor`
/// has no `is_terminal` guard of its own, unlike `raw_mode` and
/// `drain_stdin`). A test hands over `TermGuard::inert` instead, so it never
/// fights another test over the real terminal (see `TermGuard`'s own `inert`
/// field) even while driving the branch that would otherwise construct one.
fn overrides_gate(repo: &Repo) -> Result<bool> {
    overrides_gate_with(
        repo,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        Some(crate::platform::TermGuard::screen as fn() -> _),
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
/// `term` is `None` for a caller that already holds a
/// [`crate::platform::TermGuard`] of its own, and `Some` for one that does
/// not, taken only just before the first blocking read — see
/// `commands::queue::tool_requirements_gate_with` on why a second, nested
/// guard is a bug rather than merely redundant. [`overrides_gate`] always
/// passes `Some`: this project has no caller left that already holds its own
/// guard going into this.
fn overrides_gate_with(
    repo: &Repo,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    let rows = collect_override_rows(repo)?;
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
    for line in overrides_lines(rows) {
        writeln!(out, "    {line}")?;
    }
    writeln!(out)?;
    Ok(())
}

/// [`print_overrides_notice`]'s rows on their own, unindented — shared with
/// the dispatch tab's popup of the same notice, [`overrides_popup`].
///
/// An override the load left out reads `ignored — <reason>` on a row of its
/// own, labelled with the step or key it set, in place of its keys. A row
/// with nothing left applying — a whole file ignored, or every step entry
/// in the patch — draws only its ignored rows, never a `0 keys` row for
/// what does not apply. The kind column's width is taken from the rows that
/// still apply alone, so a layer with an ignored entry lays out every other
/// row exactly as it did before.
fn overrides_lines(rows: &[OverrideRow]) -> Vec<String> {
    let applies = |row: &OverrideRow| {
        row.ignored.is_empty()
            || (!row.overrides.is_empty() && row.ignored.iter().all(|i| !i.fields.is_empty()))
    };
    let target_w = rows.iter().map(|r| r.target.len()).max().unwrap_or(0);
    let labels: Vec<String> = rows.iter().map(overrides_gate_kind).collect();
    let kind_w = rows
        .iter()
        .zip(&labels)
        .filter(|(row, _)| applies(row))
        .map(|(_, kind)| kind.len())
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (row, kind) in rows.iter().zip(&labels) {
        if applies(row) {
            lines.push(match row.overrides == "—" {
                true => format!("{:<target_w$}  {kind}", row.target),
                false => format!(
                    "{:<target_w$}  {kind:<kind_w$}  {}",
                    row.target, row.overrides
                ),
            });
        }
        for item in &row.ignored {
            let (step, keys) = crate::commands::ignored_columns(row, item);
            let label = if step.is_empty() { keys } else { step };
            lines.push(format!(
                "{:<target_w$}  {label:<kind_w$}  ignored — {}",
                row.target,
                item.short_reason()
            ));
        }
    }
    lines
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
/// After [`overrides_gate`], and — like it — before `Lock::acquire`: `esc`
/// here must still mean "nothing has happened yet", which is only true ahead
/// of the lock.
fn warnings_gate(repo: &Repo, pipelines: &Pipelines) -> Result<bool> {
    warnings_gate_with(
        repo,
        pipelines,
        crate::ask::interactive(),
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        Some(crate::platform::TermGuard::screen as fn() -> _),
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
fn warnings_gate_with(
    repo: &Repo,
    pipelines: &Pipelines,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    let (settings, files, problems) = cheap_sections(repo, pipelines);

    if settings.is_empty() && files.is_empty() && problems.is_empty() {
        return Ok(true);
    }

    if !interactive {
        print_warnings_notice(out, &settings, &files, &problems)?;
        return Ok(true);
    }

    let fingerprint = warnings_fingerprint(&settings, &files, &problems);
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

/// Doctor's cheap findings, sorted under the warnings screen's three
/// headings: settings, files and problems.
fn cheap_sections(repo: &Repo, pipelines: &Pipelines) -> (Vec<String>, Vec<String>, Vec<String>) {
    use crate::commands::doctor::Warning;

    let (mut settings, mut files, mut problems) = (Vec::new(), Vec::new(), Vec::new());
    for warning in crate::commands::doctor::cheap_findings(repo, pipelines, &repo.config) {
        match warning {
            Warning::Setting(text) => settings.push(text),
            Warning::File(text) => files.push(text),
            Warning::Problem(text) => problems.push(text),
        }
    }
    (settings, files, problems)
}

/// Fingerprinted on the body alone — never the header or the footer, both
/// of which are this screen's own wording rather than a fact about the
/// project — so a person who has hidden this exact set of lines is not
/// asked again merely because a later spoolway rewords the prompt beneath
/// them. Always the 80-column body `spoolway dispatch` draws, so hiding it
/// in the dispatch tab's narrower popup hides it on the CLI too.
fn warnings_fingerprint(settings: &[String], files: &[String], problems: &[String]) -> String {
    crate::skeleton::fingerprint(&warnings_lines(settings, files, problems).join("\n"))
}

/// One of the two gates bare `spoolway`'s dispatch tab asks before it starts
/// a dispatcher, drawn as a popup over the board — the same notice
/// [`overrides_gate`] and [`warnings_gate`] draw as a screen of their own
/// for `spoolway dispatch`, and hidden by the same acknowledgement, so `x`
/// in either place quiets both.
pub(crate) struct GatePopup {
    /// The boxed panel, key line included, for the board to overlay.
    pub(crate) panel: Vec<String>,
    fingerprint: String,
    overrides: bool,
}

impl GatePopup {
    /// `x`: don't ask again until what the popup names changes.
    pub(crate) fn hide(&self, repo: &Repo) -> Result<()> {
        match self.overrides {
            true => crate::overrides::ack_write(&repo.home, &self.fingerprint),
            false => crate::overrides::warnings_ack_write(&repo.home, &self.fingerprint),
        }
    }
}

/// The overrides popup, or `None` when [`overrides_gate`] would not ask:
/// no layer at all, or one already acknowledged and unmoved since.
pub(crate) fn overrides_popup(repo: &Repo) -> Result<Option<GatePopup>> {
    let rows = collect_override_rows(repo)?;
    if rows.is_empty() {
        return Ok(None);
    }
    // The same stand-in `overrides_gate_with` takes for a layer it could
    // not re-read: an empty fingerprint never matches, so it still asks.
    let fingerprint = crate::version::layer_fingerprint(repo).unwrap_or_default();
    if !crate::overrides::ack_needed(&repo.home, &fingerprint) {
        return Ok(None);
    }
    // One blank row above the rows; `panel` puts the one under them, above
    // the key line, as step 19 of the dispatch tab's mockup draws it.
    let mut body = vec![String::new()];
    body.extend(overrides_lines(&rows));
    Ok(Some(GatePopup {
        panel: crate::screen::panel(
            "overrides are active for this project",
            &body,
            "[enter] start dispatching   [esc] back   [x] don't ask again until this changes",
        ),
        fingerprint,
        overrides: true,
    }))
}

/// How wide the warnings popup wraps a finding: narrower than the 80
/// columns [`warnings_gate`]'s own screen takes, so the box and the board's
/// margins either side of it still fit the 100 columns the dispatch tab is
/// drawn to.
const WARNINGS_POPUP_WRAP: usize = 70;

/// The warnings popup, or `None` when [`warnings_gate`] would not ask:
/// nothing to say, or the same findings already hidden.
pub(crate) fn warnings_popup(repo: &Repo, pipelines: &Pipelines) -> Option<GatePopup> {
    let (settings, files, problems) = cheap_sections(repo, pipelines);
    if settings.is_empty() && files.is_empty() && problems.is_empty() {
        return None;
    }
    let fingerprint = warnings_fingerprint(&settings, &files, &problems);
    if !crate::overrides::warnings_ack_needed(&repo.home, &fingerprint) {
        return None;
    }
    let mut body = vec![String::new()];
    body.extend(warnings_lines_at(
        &settings,
        &files,
        &problems,
        WARNINGS_POPUP_WRAP,
    ));
    // One blank row above the findings; `panel` puts the one under them,
    // above the key line, as step 20 of the dispatch tab's mockup draws it.
    Some(GatePopup {
        panel: crate::screen::panel(
            "before dispatching",
            &body,
            "[enter] start dispatching   [esc] back   [x] hide until these change",
        ),
        fingerprint,
        overrides: false,
    })
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
    warnings_lines_at(settings, files, problems, 80)
}

/// [`warnings_lines`], wrapped to `width` rather than 80 columns — the
/// dispatch tab's popup draws the same body inside a box.
fn warnings_lines_at(
    settings: &[String],
    files: &[String],
    problems: &[String],
    width: usize,
) -> Vec<String> {
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
            lines.extend(wrap_indent(text, "  ", width));
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
/// A distinct error type, downcast out of the three checks' `?` — plain `?`
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
/// - `repo.checkout` — the checkout the dispatcher actually ran in, and what
///   `Herdr` hands herdr as `--cwd` — is itself a linked worktree. herdr
///   refuses a `--cwd` that is a linked worktree with
///   `linked_worktree_source` (verified against a live herdr; see
///   `Herdr::anchor`'s doc), and the first cut of a task's workspace,
///   `Mux::create_workspace`, has nowhere to fall back to.
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
        // `Mux::create_workspace`, a task's first cut, propagates a refused
        // anchor with no fallback; the other two routes, `Mux::create_pane`
        // (a borrowed checkout) and the default `Mux::reopen_owned_pane`
        // (which is exactly `create_pane`), each already fall back to a plain
        // `workspace create` when herdr refuses the anchor they tried first —
        // but a checkout the anchor is genuinely wrong for is refused here
        // regardless of which of the three a given task would have hit, so
        // every task started on it fails or degrades the same way rather than
        // some of them working oddly while others crash.
        //
        // A linked worktree's own top always carries a `.git` *file*
        // pointing at the common git dir; the main checkout's is a
        // directory. The same test herdr itself makes of `--cwd`.
        if repo.checkout.join(".git").is_file() {
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
        bail!("Open herdr and start spoolway there:\n\n  herdr\n  spoolway");
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

/// Refuse the whole start over any live task whose `base:` has drifted out
/// from under it — a dependent that no longer agrees with its dependency's
/// base — and over a task still waiting to be cut whose base has vanished
/// from both places a worktree could start from.
///
/// Beside [`check_task_routes`] for the same reason: found and fixed by a
/// person reading this line before the lock is taken, not discovered mid-run
/// by whichever lane happens to reach it first. [`crate::dispatch::base_problem`]
/// is the one place the checks themselves live, shared with the running
/// dispatcher's own per-pass hold, so the fact and the fix never drift
/// between the two.
fn check_task_bases(repo: &Repo, tasks: &[Task]) -> Result<()> {
    // Shared across the whole batch, not rebuilt per task — see
    // [`crate::dispatch::base_problem`]'s own doc for why a chain sharing
    // one `base` must cost one `git ls-remote`, not one per task on it.
    let mut remote_cache = crate::dispatch::RemoteCache::new();
    for task in tasks {
        if let Some((reason, fix)) = crate::dispatch::base_problem(repo, task, &mut remote_cache) {
            bail!("refusing to start: {reason}\n  {fix}\n\nNothing was dispatched.");
        }
    }
    Ok(())
}

/// Settle the books on what the run was holding.
///
/// Both ways a dispatcher ends come through here — the queue emptying, and a
/// person asking it to stop — because they leave the same things behind and
/// differ only in how much of it there is. Nothing is torn down either way:
/// see [`crate::dispatch::Dispatcher::sweep_on_stop`].
///
/// A settle that fails is reported and not raised: the run is over either
/// way, and an unbanked lane is something to catch up by hand, not a reason
/// to exit non-zero.
fn stop(repo: &Repo, pipelines: &Pipelines, mux: &dyn crate::mux::Mux) -> Result<()> {
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

    /// The dispatch tab's step-30 popup body, from the ceiling a pass
    /// reports: the key's own figures, not the run log's long sentence.
    #[test]
    fn a_screen_run_names_the_ceiling_it_reached_as_the_tab_draws_it() {
        let ceiling = crate::dispatch::Ceiling {
            note: "this run has spent $5.02 against a max_cost_usd of $5.00 — …".to_string(),
            reached: "unattended.max_cost_usd reached: $5.02 of $5.00".to_string(),
        };
        assert_eq!(
            screen_stop_reason(&ceiling),
            "unattended.max_cost_usd reached: $5.02 of $5.00\n\
             Every task is where its last lane left it."
        );
    }

    /// A dispatcher already running for this repo must send `spoolway
    /// dispatch` out with [`EXIT_ALREADY_RUNNING`], never through
    /// [`crate::lock::Lock::acquire`] — a second start names the run, it
    /// never joins it.
    ///
    /// Proved indirectly rather than by mocking `Lock::acquire`: this process
    /// takes the lock itself first, the same way a real dispatcher would, and
    /// then calls `dispatch` again against the same repo. `Lock::acquire`
    /// refuses a second holder outright — see its own doc comment — so a
    /// second acquire attempt here would surface as this call returning an
    /// error naming "already running", not as a silent takeover.
    #[test]
    fn a_dispatch_finding_the_lock_held_never_takes_it() {
        let (repo, _root_guard) = fixture("lock-held-never-taken");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

        let args = DispatchArgs::default();
        let result = dispatch(&repo, &Pipelines::builtin(), &args);

        assert!(
            result.is_ok(),
            "a second `Lock::acquire` on the path `dispatch` took would have failed instead of \
             returning cleanly: {result:?}"
        );
    }

    /// The same line every time, with no count behind it — five plain,
    /// back-to-back calls against a repo whose lock another dispatcher
    /// already holds all read exactly `Dispatcher already running`, the
    /// fifth exactly like the first.
    #[test]
    fn a_held_lock_is_refused_the_same_way_every_time() {
        let (repo, _root_guard) = fixture("repeated-refusal");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

        let args = DispatchArgs::default();

        for attempt in 1..=5 {
            assert_eq!(
                dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
                EXIT_ALREADY_RUNNING,
                "attempt {attempt} of 5 should be refused the same way as every other"
            );
        }
    }

    /// An empty queue is an ordinary ending, exit code 3 — distinct from a
    /// lock already held so a caller restarting this in a loop can tell the
    /// two apart.
    #[test]
    fn an_empty_queue_exits_three() {
        let (repo, _root_guard) = fixture("empty-queue-exit");
        let args = DispatchArgs::default();
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

    /// A dependent whose `base:` no longer agrees with its dependency's —
    /// one of them edited by hand after the batch was sent, since
    /// `validate_batch` already enforced this once — refuses the whole
    /// start, naming both tasks and both bases, and the fix.
    #[test]
    fn check_task_bases_refuses_a_dependent_that_disagrees_with_its_dependency() {
        let (repo, _root_guard) = fixture("bases-chain-disagrees");
        crate::commands::testutil::add(&repo, "cart-totals", &[]);
        crate::commands::testutil::add(&repo, "cart-discounts", &["cart-totals"]);
        let mut discounts = repo.task("cart-discounts").unwrap();
        discounts.front.base = Some("main".to_string());
        discounts.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let err = check_task_bases(&repo, &tasks).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(
                "refusing to start: task `cart-discounts` is based on `main` but depends on \
                 `cart-totals`, which is based on `plan/demo`"
            ),
            "{message}"
        );
        assert!(
            message.contains("A chain has one base. Set `base:` in cart-discounts to `plan/demo`."),
            "{message}"
        );
        assert!(message.contains("Nothing was dispatched."), "{message}");
    }

    /// A cut task with no dependency whose `starts_from:` differs from its
    /// `base:` starts the dispatcher: a person may set where a task starts
    /// apart from where it lands, so the two differing is no edit to refuse.
    #[test]
    fn check_task_bases_passes_a_cut_task_that_starts_apart_from_its_base() {
        let (repo, _root_guard) = fixture("bases-cut-starts-apart");
        crate::commands::testutil::add(&repo, "cart-totals", &[]);
        let mut totals = repo.task("cart-totals").unwrap();
        totals.front.starts_from = Some("plan/demo".to_string());
        // A task is cut when it has a worktree, whatever `starts_from:` says.
        totals.front.worktree_path = Some(repo.root.clone());
        totals.front.base = Some("main".to_string());
        totals.save().unwrap();

        let tasks = repo.tasks().unwrap();
        assert!(check_task_bases(&repo, &tasks).is_ok());
    }

    /// A task still waiting to be cut whose base has vanished from both
    /// places a worktree could start from refuses the start too, checked
    /// locally before `origin` is ever asked.
    #[test]
    fn check_task_bases_refuses_a_task_whose_base_exists_nowhere() {
        let (repo, _root_guard) = fixture("bases-base-gone");
        crate::commands::testutil::add(&repo, "cart-totals", &[]);
        let mut totals = repo.task("cart-totals").unwrap();
        totals.front.base = Some("task/gh-412-checkout".to_string());
        totals.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let err = check_task_bases(&repo, &tasks).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(
                "refusing to start: task `cart-totals` is based on `task/gh-412-checkout`, \
                 which exists neither locally nor on `origin`"
            ),
            "{message}"
        );
        assert!(
            message.contains("Set `base:` in cart-totals to a branch that exists."),
            "{message}"
        );
    }

    /// A cut task is not asked to prove its base still exists — its worktree
    /// is already there, and a base that merges away afterward is not an
    /// error (non-goal).
    #[test]
    fn check_task_bases_does_not_ask_existence_of_a_task_already_cut() {
        let (repo, _root_guard) = fixture("bases-cut-not-asked");
        crate::commands::testutil::add(&repo, "cart-totals", &[]);
        let mut totals = repo.task("cart-totals").unwrap();
        totals.front.starts_from = Some("plan/demo".to_string());
        // A task is cut when it has a worktree, whatever `starts_from:` says.
        totals.front.worktree_path = Some(repo.root.clone());
        totals.save().unwrap();

        let tasks = repo.tasks().unwrap();
        assert!(check_task_bases(&repo, &tasks).is_ok());
    }

    /// A batch where every base is consistent and reachable passes through
    /// untouched.
    #[test]
    fn check_task_bases_passes_a_consistent_chain() {
        let (repo, _root_guard) = fixture("bases-consistent");
        crate::commands::testutil::add(&repo, "cart-totals", &[]);
        crate::commands::testutil::add(&repo, "cart-discounts", &["cart-totals"]);

        let tasks = repo.tasks().unwrap();
        assert!(check_task_bases(&repo, &tasks).is_ok());
    }

    /// A lock already held exits 4, whether or not it is the first start to
    /// find it that way.
    #[test]
    fn a_lock_already_held_exits_four() {
        let (repo, _root_guard) = fixture("lock-held-exit");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        let args = DispatchArgs::default();
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
            EXIT_ALREADY_RUNNING
        );
    }

    /// `spoolway dispatch` refuses the same way while the screen's own
    /// lock, not the dispatcher's, names a live process — the screen holds
    /// `spoolway.pid` for as long as it is open, with no dispatcher of its
    /// own running yet.
    #[test]
    fn a_live_screen_lock_refuses_a_typed_dispatch() {
        let (repo, _root_guard) = fixture("screen-lock-refuses");
        let _lock = crate::lock::Lock::acquire(&repo.screen_lock_file(), false, None).unwrap();
        let args = DispatchArgs::default();
        assert_eq!(
            dispatch(&repo, &Pipelines::builtin(), &args).unwrap(),
            EXIT_ALREADY_RUNNING
        );
    }

    /// The child bare `spoolway`'s dispatch tab starts (`--from-screen`)
    /// must not be refused against its own parent's `spoolway.pid` — see
    /// [`already_running`]. With no other dispatcher up, it goes on to
    /// take the dispatcher's own lock and run.
    #[test]
    fn a_screen_started_child_is_not_refused_by_its_own_parent() {
        let (repo, _root_guard) = fixture("screen-child-not-refused");
        let _screen_lock =
            crate::lock::Lock::acquire(&repo.screen_lock_file(), false, None).unwrap();
        assert!(!already_running(&repo, true).unwrap());
        assert!(already_running(&repo, false).unwrap());
    }

    /// A `spoolway.pid` or `dispatch.pid` naming a process that has already
    /// exited is stale, not a live holder — the same rule [`crate::lock::Lock::holder`]
    /// already applies to the dispatcher's own lock.
    #[test]
    fn a_dead_process_in_either_lock_does_not_refuse() {
        let (repo, _root_guard) = fixture("dead-holder-not-refused");
        std::fs::write(repo.screen_lock_file(), "0\n").unwrap();
        std::fs::write(repo.lock_file(), "0\n").unwrap();
        assert!(!already_running(&repo, false).unwrap());
    }

    /// A pipeline whose only step names no `run:` at all — standing in for a
    /// project whose pipeline never reaches `spoolway stack`.
    fn single_step_pipelines() -> Pipelines {
        let pipeline: Pipeline =
            serde_norway::from_str("steps:\n  - id: a\n    run: x\n    on_pass: done\n").unwrap();
        let mut pipelines = std::collections::BTreeMap::new();
        pipelines.insert("default".to_string(), pipeline);
        Pipelines {
            pipelines,
            ignored_overrides: Vec::new(),
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
        let (mut repo, _root_guard) = fixture("git-identity-nothing-commits");
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
        let (repo, _root_guard) = fixture("git-identity-missing");
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
        let (repo, _root_guard) = fixture("git-identity-auto-commit-alone");
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
        let (repo, _root_guard) = fixture("git-identity-set");
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
        let (mut repo, _root_guard) = fixture("git-identity-checkout-config");
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
        let (repo, _root_guard) = fixture("index-lock");
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
    fn bare_repo(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let dir = crate::scratch::root(&format!("bare-checkout-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        crate::repo::run(&dir, "git", &["init", "-q", "--bare"]).unwrap();
        (
            Repo {
                borrowed: false,
                root: dir.to_path_buf(),
                checkout: dir.to_path_buf(),
                config: Config::default(),
                home: dir.join(".home"),
            },
            dir,
        )
    }

    /// A repo dispatched from a linked worktree — `checkout` a sibling
    /// worktree cut off `root`, `root` the main checkout beside it — paired
    /// with a closure that removes the worktree and the scratch tree
    /// afterwards.
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
            borrowed: false,
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
    /// `name`/`is_available` for real — the only two
    /// [`check_backend_checkout`] ever reads — and refusing every other call
    /// outright, so a test that somehow reached one fails loudly rather than
    /// doing something real.
    struct StubMux {
        name: &'static str,
        available: bool,
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
        let (repo, _root_guard) = bare_repo("non-herdr");
        let mux = StubMux {
            name: "headless",
            available: true,
            in_own_pane: true,
        };
        assert!(check_backend_checkout(&repo, &mux).unwrap().is_some());
    }

    /// herdr against an ordinary checkout — the fixture's own — resolves a
    /// main checkout fine and passes.
    #[test]
    fn backend_checkout_passes_herdr_on_an_ordinary_checkout() {
        let (repo, _root_guard) = fixture("herdr-ordinary-checkout");
        let mux = StubMux {
            name: "herdr",
            available: true,
            in_own_pane: true,
        };
        assert!(check_backend_checkout(&repo, &mux).unwrap().is_some());
    }

    /// herdr against a bare repository — no main checkout `main_checkout`
    /// can place at all — is refused, naming the path and a way out.
    #[test]
    fn backend_checkout_refuses_herdr_on_a_bare_repository() {
        let (repo, _root_guard) = bare_repo("herdr-bare");
        let mux = StubMux {
            name: "herdr",
            available: true,
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
    /// main checkout beside it — is refused. Naming both
    /// checkouts and no backend to switch to is the acceptance criterion in
    /// full: unlike the bare-repository refusal above, there is no other
    /// backend that would fix this, so none is offered.
    ///
    /// A task's first cut, `Mux::create_workspace`, hands
    /// `open_worktree_workspace` no fallback at all: a `--cwd` herdr refuses
    /// fails that task outright. See `check_backend_checkout`'s own doc, above
    /// it in this module, for why the other two routes are still refused the
    /// same checkout here even though each degrades on its own rather than
    /// failing outright.
    #[test]
    fn backend_checkout_refuses_herdr_on_a_dispatcher_started_in_a_linked_worktree() {
        let (repo, main, cleanup) = linked_worktree_repo("split");
        let mux = StubMux {
            name: "herdr",
            available: true,
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

    /// A herdr run with no pane to draw in is refused, naming the way in —
    /// `herdr` and then `spoolway` — with no exemption, and no longer saying
    /// that a dispatcher has to be visible: the dispatch tab shows this
    /// refusal as its popup, word for word.
    #[test]
    fn dispatcher_visible_refuses_herdr_outside_a_pane() {
        let mux = StubMux {
            name: "herdr",
            available: true,
            in_own_pane: false,
        };
        let err = format!("{:#}", check_dispatcher_visible(&mux).unwrap_err());
        assert_eq!(
            err,
            "Open herdr and start spoolway there:\n\n  herdr\n  spoolway"
        );
    }

    /// A herdr run that is in a pane passes straight through.
    #[test]
    fn dispatcher_visible_passes_herdr_in_a_pane() {
        let mux = StubMux {
            name: "herdr",
            available: true,
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
        let (repo, _root_guard) = fixture("overrides-gate-no-layer");
        assert!(overrides_gate(&repo).unwrap());
    }

    /// A fixture with a real layer on it, forked the same way `spoolway
    /// pipeline override` writes one — for every `overrides_gate_with` case
    /// below, which needs an actual `OverrideRow` to ask about.
    fn fixture_with_layer(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, root_guard) = fixture(name);
        std::fs::create_dir_all(repo.overrides_dir().join("pipelines")).unwrap();
        std::fs::write(
            repo.overrides_dir().join("pipelines/default.yml"),
            "steps:\n  implement:\n    model: fake-opus\n",
        )
        .unwrap();
        (repo, root_guard)
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
        let (repo, _root_guard) = fixture_with_layer("overrides-gate-no-tty");
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
        let (repo, _root_guard) = fixture_with_layer("overrides-gate-enter");
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
        let (repo, _root_guard) = fixture_with_layer("overrides-gate-esc");
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
        let (repo, _root_guard) = fixture_with_layer("overrides-gate-x");
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
            ignored: Vec::new(),
        };
        assert_eq!(overrides_gate_kind(&two_keys), "2 keys");

        let one_key = OverrideRow {
            target: "config.toml".into(),
            kind: "patch",
            overrides: "agents.claude.concurrency".into(),
            ignored: Vec::new(),
        };
        assert_eq!(overrides_gate_kind(&one_key), "1 key");

        let prompt = OverrideRow {
            target: "prompts/reviewer".into(),
            kind: "whole file",
            overrides: "—".into(),
            ignored: Vec::new(),
        };
        assert_eq!(overrides_gate_kind(&prompt), "whole file");
    }

    /// An ignored override's row reads `ignored — <reason>` under the step
    /// it named, in place of its keys; every row that still applies reads
    /// byte for byte as it did with nothing ignored, and a row with nothing
    /// left applying draws no `0 keys` row of its own.
    #[test]
    fn overrides_lines_draw_an_ignored_override_as_ignored_and_leave_the_rest() {
        let ignored = |target: &str, fields: &str, reason: &str| crate::overrides::Ignored {
            target: target.into(),
            fields: fields.into(),
            reason: reason.into(),
        };
        let release = |ignored: Vec<crate::overrides::Ignored>| OverrideRow {
            target: "pipelines/release.yml".into(),
            kind: "patch",
            overrides: "fix.agent, fix.model, preflight.model".into(),
            ignored,
        };
        let prompt = OverrideRow {
            target: "prompts/reviewer".into(),
            kind: "whole file",
            overrides: "—".into(),
            ignored: Vec::new(),
        };
        let before = overrides_lines(&[release(Vec::new()), prompt]);

        let prompt = OverrideRow {
            target: "prompts/reviewer".into(),
            kind: "whole file",
            overrides: "—".into(),
            ignored: Vec::new(),
        };
        let gone = OverrideRow {
            target: "pipelines/gone.yml".into(),
            kind: "patch",
            overrides: String::new(),
            ignored: vec![crate::overrides::Ignored::missing_pipeline("gone")],
        };
        let after = overrides_lines(&[
            release(vec![ignored(
                "pipelines/release.yml step `publish`",
                "publish.agent, publish.model",
                "names both `run:` and `agent:` — a step runs a process or a model, not both",
            )]),
            prompt,
            gone,
        ]);

        assert_eq!(after[0], before[0], "the row still applying is unchanged");
        assert_eq!(
            after[1],
            "pipelines/release.yml  step publish  ignored — names both `run:` and `agent:`"
        );
        assert_eq!(after[2], before[1], "the prompt row is unchanged");
        assert_eq!(
            after[3],
            "pipelines/gone.yml     the whole file  ignored — names pipeline `gone`, which the \
             checkout does not have"
        );
        assert_eq!(after.len(), 4, "no `0 keys` row for gone.yml: {after:#?}");
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

    /// The row `boxed` wraps a body line in, trimmed back to that line.
    fn popup_row(row: &str) -> &str {
        row.trim_matches(['│', ' '])
    }

    /// Step 19 of the dispatch tab's mockup: a blank row, the layer's rows,
    /// one blank row, the key line, and the border straight under it.
    #[test]
    fn the_overrides_popup_reads_as_the_dispatch_tab_draws_it() {
        let (repo, _root_guard) = fixture_with_layer("overrides-popup-layout");
        let panel = overrides_popup(&repo)
            .unwrap()
            .expect("a layer to name")
            .panel;
        let n = panel.len();
        assert!(
            panel[0].starts_with("┌─ overrides are active for this project "),
            "{panel:?}"
        );
        assert_eq!(popup_row(&panel[1]), "", "{panel:?}");
        assert!(
            popup_row(&panel[2]).starts_with("pipelines/default.yml"),
            "{panel:?}"
        );
        assert_eq!(popup_row(&panel[n - 3]), "", "{panel:?}");
        assert_eq!(
            popup_row(&panel[n - 2]),
            "[enter] start dispatching   [esc] back   [x] don't ask again until this changes"
        );
        assert!(
            !popup_row(&panel[n - 4]).is_empty(),
            "one blank row above the keys: {panel:?}"
        );
    }

    /// Step 20: the same shape, the key line straight on the border.
    #[test]
    fn the_warnings_popup_reads_as_the_dispatch_tab_draws_it() {
        let (mut repo, _root_guard) = fixture("warnings-popup-layout");
        repo.config.unattended.enabled = true;
        let panel = warnings_popup(&repo, &Pipelines::builtin())
            .expect("unattended with no ceiling is worth a warning")
            .panel;
        let n = panel.len();
        assert!(panel[0].starts_with("┌─ before dispatching "), "{panel:?}");
        assert_eq!(popup_row(&panel[1]), "", "{panel:?}");
        assert_eq!(popup_row(&panel[2]), "settings", "{panel:?}");
        assert_eq!(
            popup_row(&panel[n - 2]),
            "[enter] start dispatching   [esc] back   [x] hide until these change"
        );
        assert_eq!(popup_row(&panel[n - 3]), "", "{panel:?}");
        assert!(
            !popup_row(&panel[n - 4]).is_empty(),
            "one blank row above the keys: {panel:?}"
        );
    }

    /// `x` on the warnings popup quiets both surfaces: the popup is not
    /// asked again, and neither is `spoolway dispatch`'s own 80-column
    /// screen, though the popup wraps the same findings narrower.
    #[test]
    fn hiding_the_warnings_popup_hides_the_cli_screen_too() {
        let (mut repo, _root_guard) = fixture("warnings-popup-hide");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        warnings_popup(&repo, &pipelines)
            .expect("something to warn about")
            .hide(&repo)
            .unwrap();
        assert!(warnings_popup(&repo, &pipelines).is_none());

        let mut out = Vec::new();
        let proceed = warnings_gate_with(
            &repo,
            &pipelines,
            true,
            &mut keys(""),
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    /// `x` on the overrides popup does the same for the overrides gate.
    #[test]
    fn hiding_the_overrides_popup_hides_the_cli_gate_too() {
        let (repo, _root_guard) = fixture_with_layer("overrides-popup-hide");
        overrides_popup(&repo)
            .unwrap()
            .expect("a layer to name")
            .hide(&repo)
            .unwrap();
        assert!(overrides_popup(&repo).unwrap().is_none());

        let mut out = Vec::new();
        let proceed = overrides_gate_with(
            &repo,
            true,
            &mut keys(""),
            &mut out,
            Some(crate::platform::TermGuard::inert),
        )
        .unwrap();
        assert!(proceed);
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
    }

    /// A fresh project has never been initialised, so `doctor_sync`'s own
    /// notes fire and land under "files" — enough to prove this screen has
    /// something to say without a tty, and prints it without ever reading a
    /// key (`input` is left empty; a `read_key` call here would hang the
    /// test).
    #[test]
    fn warnings_gate_with_no_tty_prints_and_proceeds() {
        let (repo, _root_guard) = fixture("warnings-gate-no-tty");
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
        let (mut repo, _root_guard) = fixture("warnings-gate-enter");
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
        let (mut repo, _root_guard) = fixture("warnings-gate-esc");
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
        let (mut repo, _root_guard) = fixture("warnings-gate-x");
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
        let (mut repo, _root_guard) = fixture("warnings-gate-footer");
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
}
