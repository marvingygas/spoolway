//! `spoolway report`, and the verb that moves a settled or stuck task:
//! `resume`.

use super::*;

/// Advance a task according to what the step reported.
///
/// The prompt names an outcome; this resolves the destination from the
/// pipeline. That indirection is what keeps role prompts free of any
/// knowledge of the pipeline's shape.
///
/// `started_for` is the step the dispatcher started this lane for — read once
/// at the CLI boundary and handed in, never read from the environment here.
/// The environment is process-wide and a report is not: reading it inside this
/// function let any other spoolway in the same process decide what this task
/// was judged against, which under `cargo test` meant whichever task reported
/// while a sibling had the variable set. `None` means nobody started this — a
/// person running `spoolway report` by hand, reporting on the task as it
/// stands.
pub fn report(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &ReportArgs,
    started_for: Option<&str>,
) -> Result<()> {
    let id = resolve_task_id(args.task.as_deref())?;

    // Held across this whole read-modify-write, so a dispatcher pass that
    // read this task file before the report cannot write a stale copy back
    // after it — the lost report of review finding 2. There is no
    // multiplexer call anywhere in `report`, so the one rule this lock has
    // is kept here.
    //
    // The one slow thing under the lock is `commit_lane_work` below — a
    // local `git add -A` + `git commit` in the lane's worktree, well inside
    // `TaskLock::WAIT` in normal use. If it ever does run long, a waiting
    // dispatcher's `persist` still reload-checks `last_report` once its own
    // wait times out, so the worst case is a slower pass, not a lost report.
    //
    // Best effort: a lock a live process still holds after the wait is
    // logged, and the report proceeds unlocked rather than being refused.
    let task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(&id));
    if task_lock.is_err() {
        crate::problem_log::append(
            repo,
            &format!("{id}: task lock still held, reporting without it"),
        );
    }

    let mut task = repo.task(&id)?;
    let pipeline = pipelines.for_task(&task)?;
    let mut current = task.stage().to_string();

    // A lane started for a step that a person's own Escape (or the board's
    // `p`) parked mid-turn, with no idea any of that happened, reports in
    // against the step it actually ran — not `paused`, which is all the
    // task's own stage says now. This report is itself proof the lane is
    // alive and working, the same fact `dispatch::auto_restore_parked`
    // otherwise waits for the next pass to see off the lane list — so it is
    // undone here, ahead of the `started_for` guard below, rather than left
    // for that pass to catch. Left alone for a gate (`paused_at` set —
    // waiting on a decision, not a lane) or an escalated park
    // (`tear_down_and_escalate`'s own road), where `started_for` still
    // answers "no" below and the report is refused the way it always was.
    if current == crate::pipeline::PAUSED
        && !task.front.escalated
        && task.front.paused_at.is_none()
        && started_for.is_some_and(|step| task.front.parked_from.as_deref() == Some(step))
    {
        let parked_from = task.front.parked_from.clone().expect("checked above");
        task.front.parked_from = None;
        task.front.escalated = false;
        task.front.resume = None;
        // A stop's mark is spent by any resume, whoever sends it — see
        // `back_onto_its_step`'s own clear of the same field. The stop
        // popup's `i` parks with `parked_from` set exactly like a person's
        // own Escape does, so this shape can be either; left set, the next
        // `spoolway start` would read `resume_stop_parked` and carry this
        // task through a gate with nobody there to answer it.
        task.front.parked_by_stop = false;
        task.set_stage_unbanked(
            &parked_from,
            "a late report landed on the step it parked from",
        );
        current = parked_from;
    }

    // A lane may only report on the step it was started for.
    //
    // Without this, a second report from one lane is applied to whatever step
    // the task moved to on the first — which advances it again, and the step in
    // between never starts a lane at all. Every report says `pass`, the task
    // reaches `done`, and the only trace is a missing log file.
    //
    // Found in a live run: an archivist reported twice at `document`, so the
    // step after it was credited with a pass it never ran. That step is
    // `handover` — the one that opens the pull request and, on the plan's last
    // open task, merges the stack — so the change was archived with nothing
    // pushed anywhere, the same damage as `handover` skipping the work, from a
    // step that never ran it.
    //
    // `started_for` is the dispatcher's own word for what this lane is, set
    // for every lane it starts. Absent means nobody started this: a person
    // running `spoolway report` by hand is reporting on the task as it stands,
    // which is exactly what the stage says.
    if let Some(started_for) = started_for
        && !started_for.is_empty()
        && started_for != current
    {
        bail!(
            "this lane was started for `{started_for}`, but task `{id}` is at \
                 `{current}` now — so this report is about a step the task has already \
                 left, and applying it would advance the task past `{current}` without \
                 anything having run it.\n\n\
                 A lane reports once, at the end of its turn. If you have already \
                 reported, your turn is over and there is nothing more to do."
        );
    }

    // The four flags share one clap `group` (see `cli::ReportArgs`), so clap
    // refuses any command line that gives more than one of them before this
    // runs. The ordered match is therefore dead defence, not a precedence
    // rule anyone relies on — it is kept only so that loosening that group
    // later fails closed on a known order rather than on a random arm. When
    // exactly one is set the order does not matter; when none is, the last
    // arm reports the omission.
    let outcome = match (args.pass, args.fail, args.block, args.pause) {
        (true, _, _, _) => Outcome::Pass,
        (_, true, _, _) => Outcome::Fail,
        (_, _, true, _) => Outcome::Block,
        (_, _, _, true) => Outcome::Pause,
        _ => bail!("say what happened: --pass, --fail, --block, or --pause"),
    };

    // `--pause` says nothing short of a person can clear this, which only
    // means anything on `blocked` itself — every other step still has
    // `--fail` and `--block` for exactly that fact. Refused here, at the top,
    // rather than left to fall through to whatever `step.destination` would
    // have made of an outcome no other step's graph was ever asked to draw an
    // edge for.
    if outcome == Outcome::Pause && current != crate::pipeline::BLOCKED {
        bail!(
            "--pause only means anything on `{blocked}` — task `{id}` is at `{current}`, which \
             still has `--fail` and `--block` to say the same thing",
            blocked = crate::pipeline::BLOCKED,
        );
    }

    // `--stage` names where a pass from `blocked` lands, in place of
    // `cleared_block_target`'s own answer — refused the same way `--pause`
    // is, by name, on every step but `blocked`, and only for a `--pass`:
    // naming a destination for `--fail`, `--block` or `--pause` off
    // `blocked` would be asking a report that already has nowhere to go
    // (see `paused_from_blocked` in [`route`]) to pick one anyway.
    //
    // Two distinct ways to trip this, told apart so neither refusal reads
    // as contradicting itself: off `blocked` entirely, naming the step this
    // report actually is at gives a lane somewhere to go from here (a plain
    // `--pass`, same as `--pause`'s own refusal above); on `blocked` itself
    // with anything but a `--pass`, naming the step again would read as
    // "you are at blocked, but --stage only works from blocked" — so this
    // one names the *outcome* that is wrong instead.
    if args.stage.is_some() && current != crate::pipeline::BLOCKED {
        bail!(
            "--stage only means anything on a `--pass` from `{blocked}` — task `{id}` is at \
             `{current}`, which has no `--stage` to give: report a plain `--pass` instead",
            blocked = crate::pipeline::BLOCKED,
        );
    }
    if args.stage.is_some() && current == crate::pipeline::BLOCKED && outcome != Outcome::Pass {
        bail!(
            "--stage only means anything on a `--pass` from `{blocked}` — this report is a \
             `--{outcome}`, not a `--pass`",
            blocked = crate::pipeline::BLOCKED,
        );
    }

    // What this step wants the next one to know, credited to the step that
    // said it — written whatever the outcome, since a lane that blocked can
    // still have learned something worth leaving behind. Appended one line
    // per `--handoff`, in the same save that routes the task, so a dependent
    // task's `spoolway queue show` never finds the note without the routing
    // that made it current.
    for handoff in &args.handoff {
        task.append_to_section(
            "## Handoff",
            &format!("- `{current}` — {}\n", handoff.trim().replace('\n', " ")),
        );
    }

    // Nothing here checks what a lane wrote against a path list any more —
    // `blocked_on_write` and `blocked_on_overreach` are retired: both only
    // ever matched this project's own tracked files, since
    // `git status --porcelain` never lists anything git-ignored, and every
    // runtime file either check named was git-ignored. Confinement is now
    // the person's own agent settings, outside this repository — see
    // `docs/concepts.md`'s Reach section.
    let unattended = repo.unattended();

    let dependents = crate::dispatch::queue_dependents(repo, &task)?;
    let routed = route(
        &mut task,
        pipeline,
        &current,
        outcome,
        unattended,
        args.stage.as_deref(),
        dependents,
    )?;
    let mut destination = routed.destination;
    let gated = routed.gated;
    let pause_note = routed.pause_note;
    let resumed = routed.resumed;
    let paused_from_blocked = routed.paused_from_blocked;
    let gated_at = routed.gated_at;

    // Committing is deterministic, so it is not left to a model. Every lane
    // goes through this command — that is the pipeline's central contract,
    // not a convention — which makes it the one place that covers a change
    // however it was made: an edit, a `sed -i`, a formatter, a generated
    // file. A hook on the editing tools would miss everything Bash does,
    // and a hook at end of turn would have to be written once per agent
    // kind.
    let commit_note = commit_lane_work(repo, &id, &current);
    if let Some(note) = commit_note.as_ref().and_then(AutoCommit::note) {
        task.log_status(note);
    }

    // Reaching the reserved `done` stage tears the worktree down on arrival
    // (see `dispatch::route_reserved_stage`). It must not run while the tree
    // holds work `auto_commit` above could not record: that terminal "cannot
    // delete work that was never recorded" (review finding 4), and a git
    // failure leaves exactly that. Held at `blocked` instead, so a person
    // sees the worktree before it is gone.
    //
    // Only `Unrecorded` — not residue a lane deliberately left, which is named
    // in the status log and backed up nowhere. Cleanup knowingly discards that
    // residue rather than committing files the lane excluded or stranding the
    // task. `dispatch::Dispatcher::clean_up` makes
    // the same check for the road every shipped pipeline actually takes to
    // `done`, from a command step rather than from this report; this covers a
    // pipeline whose agent step routes `on_pass: done` directly.
    let routes_to_cleanup = destination == crate::pipeline::DONE;
    let held_dirty =
        routes_to_cleanup && commit_note.as_ref().is_some_and(AutoCommit::is_unrecorded);
    if held_dirty {
        task.log_status(&format!(
            "`{current}` reported `{outcome}`, but the worktree holds work that could \
             not be committed — holding at `{}` rather than letting `{destination}` tear \
             it down",
            crate::pipeline::BLOCKED,
        ));
        destination = crate::pipeline::BLOCKED.to_string();
        set_blocked_from(&mut task, &current);
    }

    // Left for the dispatcher to bank into this lane's ledger line. Recorded
    // with the step it belongs to, so that a later lane which dies without
    // reporting is banked as having no outcome rather than inheriting this one.
    task.front.last_report = Some(crate::task::LastReport {
        step: current.clone(),
        outcome: outcome.as_str().to_string(),
        at: chrono::Utc::now().timestamp(),
        blocked: destination == crate::pipeline::BLOCKED,
    });

    let arrival = crate::dispatch::with_walk_note(
        pause_note.as_deref().or(args.message.as_deref()),
        routed.walked_past,
    );
    task.set_stage(&destination, arrival.as_deref());
    // The line `set_stage` just wrote is spoolway's own arrival note, credited
    // to no step, the same as every other arrival. A gate is the one road
    // where that note replaces the lane's own `-m` rather than carrying it —
    // and the lane's account of its own pass is real work, so it is not
    // thrown away: it lands as a second line here, credited to `current`,
    // the step it reported from.
    if pause_note.is_some()
        && let Some(message) = args.message.as_deref()
        && !message.trim().is_empty()
    {
        task.log_status(&format!(
            "`{current}`: {}",
            message.trim().replace('\n', " ")
        ));
    }
    task.save()?;

    // Printed only now the task file is on disk: a note about a commit that
    // was made, next to a save that then failed, would be a lie in the log
    // (review finding 40).
    if let Some(note) = commit_note.as_ref().and_then(AutoCommit::note) {
        println!("{note}");
    }

    match &resumed {
        // Named for what it is, rather than printed as an ordinary transition.
        // `implement --block--> implement` in a log reads as a step that routed
        // to itself, which is not a thing a pipeline can even declare — what
        // happened is that the run had nobody to stop for.
        Some(target) => println!(
            "{id}: {current} --{outcome}--> resumed at {target} (unattended: \
             nobody to block for)"
        ),
        // Said in full, because the lane that reported is about to read this
        // and has no other way of knowing why the task did not move on. It is
        // not a refusal of the report and there is nothing to do about it —
        // true of a caught fail or block exactly as it is of a caught pass,
        // since `gate_at` holds this step's whole outcome, not only a pass.
        //
        // `stop_choices` prints `spoolway resume <id>` here now — that is
        // still refused when it is the lane's own turn that types it
        // (`refuse_from_lane`, unmoved by any of this), but this pane
        // belongs to whoever reads it next, and every choice a stop offers
        // is printed key first, then the command, the same as the board's
        // own row for this task will read the moment it redraws.
        // `current` alone does not say which step's gate this is when the
        // hold came from an unblocker's pass — `current` is `blocked` there,
        // not the step the person actually has to answer for — so
        // `gated_at` names it.
        None if gated => {
            let at = gated_at
                .as_deref()
                .map(|step| format!(", at `{step}`'s gate"))
                .unwrap_or_default();
            println!(
                "{id}: {current} --{outcome}--> {} - held here for a person{at}{}",
                crate::pipeline::PAUSED,
                stop_choices(&id, &current)
            )
        }
        // `blocked` had nowhere else to send this — see `paused_from_blocked`
        // above — so the lane that reported it, and anyone reading the log
        // afterwards, needs telling why the destination is `paused` rather
        // than another lap of `blocked`.
        None if paused_from_blocked => println!(
            "{id}: {current} --{outcome}--> {} - nothing here could clear it{}",
            crate::pipeline::PAUSED,
            stop_choices(&id, &current)
        ),
        // The routed destination was a cleanup terminal, and the worktree
        // still has uncommitted work in it — see `held_dirty` above.
        None if held_dirty => println!(
            "{id}: {current} --{outcome}--> {} — the worktree still has uncommitted work; \
             a person should look before it is cleaned up",
            crate::pipeline::BLOCKED
        ),
        None => println!("{id}: {current} --{outcome}--> {destination}"),
    }
    Ok(())
}

/// What [`route`] decided, for [`report`] to act on once it is back with a
/// `Repo` and a clock to finish the turn with.
pub struct Routed {
    /// The step the task now sits on.
    pub destination: String,
    /// Whether a gate — the step's own `gate: true` or the task's own
    /// `gate_at` — is what parked this at `paused` rather than the ordinary
    /// graph. See [`gate_hold`].
    pub gated: bool,
    /// The status-log wording a gate wants for the arrival line, in place of
    /// the lane's own `-m` message there, if one caught this report — the
    /// lane's own message still lands as a second line; see [`report`].
    pub pause_note: Option<String>,
    /// The step this run resumed itself onto, when nobody was staffing
    /// `blocked` to park in front of. `None` on every other road out.
    pub resumed: Option<String>,
    /// Whether `current` was `blocked` itself and the outcome was anything
    /// but a pass — the one case with no second destination, parked on
    /// `paused` instead. [`report`]'s own final message tells this apart
    /// from an ordinary park.
    pub paused_from_blocked: bool,
    /// The step whose own gate held an unblocker's `--pass`, when that is
    /// what `gated` above means — `None` for every other kind of gate, an
    /// ordinary step's own pass among them, where the pane a person reads is
    /// already that step's own and needs no further naming. [`report`]'s
    /// final message adds the clause this names; nothing else reads it.
    pub gated_at: Option<String>,
    /// The Status Log wording for the hidden steps this move followed
    /// `on_pass` past on its way to `destination`, if it passed any — see
    /// [`crate::dispatch::land_past_hidden`]. [`report`] puts it on the
    /// arrival line, after any message the move carries.
    pub walked_past: Option<String>,
}

/// The routing decision, lifted out of [`report`] so a test can walk every
/// outcome at every step through the same function `spoolway report` itself
/// calls — no `Repo`, no filesystem, no clock, so the walk that proves it
/// bounded can run entirely in `cargo test`.
///
/// `task` is mutated in place: routing is not only a lookup, it is also
/// where leaving `blocked` starts every loop count again
/// ([`Task::reset_loop_counts`]), where
/// a spent loop logs why it gave up (`apply_loop_budget`), and where a gate
/// stamps `paused_at`/`paused_by`. All of that belongs to the routing
/// decision and moves with it — only the parts of [`report`] that need a
/// real repository (committing the worktree, saving the file, firing the
/// tracking hook) stay behind.
///
/// `dependents` is how many open tasks above this one in its group hold a
/// `last:` step back ([`crate::dispatch::same_group_dependents`]). Routing has
/// no repository to count them in, so the caller does. The destination is
/// followed past every step this task does not run, before the `loop:` check,
/// so it is the step the task lands on that spends the arrival.
pub fn route(
    task: &mut Task,
    pipeline: &Pipeline,
    current: &str,
    outcome: Outcome,
    unattended: bool,
    stage: Option<&str>,
    dependents: usize,
) -> Result<Routed> {
    // `queued` and `paused` are held states no lane works at, so a report
    // filed from one has no step to settle. Said outright: the step lookup
    // below would answer that "`queued` is not a step", which reads as a typo
    // in a name the reporter never typed.
    if current == crate::pipeline::QUEUED || current == crate::pipeline::PAUSED {
        let instead = match current == crate::pipeline::QUEUED {
            true => "it starts on its own when the dispatcher reaches it".to_string(),
            false => format!(
                "to release it, run `spoolway resume {}` or press `r` on the board",
                task.id()
            ),
        };
        bail!(
            "task `{}` is {current}, which no lane works at, so there is no step to report \
             on — {instead}",
            task.id()
        );
    }
    let step = pipeline.require_step(current).with_context(|| {
        format!(
            "task `{}` is on a step that this pipeline does not define",
            task.id()
        )
    })?;

    // `blocked` itself is never reported on by the ordinary graph below: a
    // pass is special-cased, and everything else — `--fail`, `--block`, and
    // the new `--pause` — is "not a pass" here, and used to have nowhere else
    // to go but back onto `blocked`, unbounded. There is nobody left to hand a
    // repeat of that to but a person, so it borrows `gate:`'s own shape: see
    // the branch below.
    let paused_from_blocked = current == crate::pipeline::BLOCKED && outcome != Outcome::Pass;
    let mut gated_at = None;
    let mut walked_past = None;

    let mut destination = if current == crate::pipeline::BLOCKED && outcome == Outcome::Pass {
        // Where this task actually stopped, read once before anything below
        // mutates the fields that answer it — the step whose work this pass
        // stands in for, and so the step whose gate answers for it, never
        // `blocked`'s own. Both the `--stage` refusal and the gate hold
        // below need it.
        let origin = resume_target(task, pipeline);

        // `blocked` declares no `on_pass` of its own — where its pass goes is
        // read from the task's own record of where it stopped, by
        // `cleared_block_target`. For an agent step, that is one step *past*
        // there rather than back onto it: the unblocker did that step's work,
        // so arriving back on it would pay for the work twice — a `--pass`
        // from `blocked` is taken at its word, unconditionally, which is what
        // the `true` below means. A command step has no such trade — see
        // `cleared_block_target`'s own doc for why it is always handed back
        // to itself.
        //
        // Either way this goes through the same `resume_at` that
        // `spoolway resume` performs by hand, and both start every `loop:`
        // count again before anything is checked (`resume_at` itself refunds
        // nothing; leaving `blocked` is what does). What the two share is the
        // mark: the lane that originally hit the
        // block — which had often already read the tree and done most of the
        // work — is continued rather than replaced by a cold one.
        //
        // `stage` overrides `cleared_block_target`'s own answer when the
        // unblocker names one — bounded by `steps_run`, the steps this task
        // has actually run a lane at, so a `--stage` can send it back onto
        // ground already covered but never somewhere it was never staffed.
        // And never past a gate: `gate_between` reads the same pipeline
        // order `steps_run` already bounds this to, from `origin` (inclusive,
        // so naming the gate step itself is still allowed — it runs again
        // and its own gate holds its own pass) up to but not including
        // `named` (exclusive, so naming a step that is itself gated is never
        // refused for being "past" its own gate). Checked first, ahead of
        // `steps_run`'s own bound below: a `--stage` naming a step this task
        // has genuinely never run is still a step past an unanswered gate,
        // and the gate is the more useful refusal to give — it says what is
        // actually in the way, where "has never been at" would read as a
        // typo rather than a wall.
        let target = match stage {
            Some(named) => {
                let run = steps_run(task, pipeline);
                if let Some(gate_step) = gate_between(pipeline, &origin, named) {
                    // Only the steps at or before the gate itself — a step
                    // past it is exactly what this refusal exists to keep
                    // `--stage` from naming, so listing it back as an
                    // alternative would contradict the refusal in the same
                    // breath.
                    let gate_idx = pipeline.steps.iter().position(|s| s.id == gate_step);
                    let allowed: Vec<&String> = run
                        .iter()
                        .filter(|s| {
                            let idx = pipeline.steps.iter().position(|step| step.id == **s);
                            matches!((idx, gate_idx), (Some(i), Some(g)) if i <= g)
                        })
                        .collect();
                    // `origin` is what this task's pass stands in for;
                    // `gate_step` is whose gate answers for it — the same
                    // step whenever `origin` is itself gated, which is the
                    // common case, but not always: an ungated `origin` can
                    // still have a gate further on, between it and `named`.
                    let subject = if origin == gate_step {
                        format!("task `{}` stopped at `{origin}`, which is gated", task.id())
                    } else {
                        format!(
                            "task `{}` stopped at `{origin}`, and `{gate_step}` after it is \
                             gated",
                            task.id()
                        )
                    };
                    // Nothing to list when the task has run no step at or before
                    // the gate: ending on "before it: " with nothing after it
                    // reads as a message cut off.
                    let options = match allowed.is_empty() {
                        true => format!(
                            "This task has not run `{gate_step}` or any step before it, so \
                             there is none to name — report a plain `--pass` without \
                             `--stage` instead"
                        ),
                        false => format!(
                            "Name `{gate_step}` or a step before it: {}",
                            allowed
                                .iter()
                                .map(|s| format!("`{s}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    };
                    bail!(
                        "{subject} - a pass from `{blocked}` may not name a step past it. \
                         {options}",
                        blocked = crate::pipeline::BLOCKED,
                    );
                }
                if !run.iter().any(|s| s == named) {
                    bail!(
                        "task `{}` has never been at `{named}` - a pass from `{blocked}` may \
                         name a step this task has already run: {}",
                        task.id(),
                        run.iter()
                            .map(|s| format!("`{s}`"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        blocked = crate::pipeline::BLOCKED,
                    );
                }
                named.to_string()
            }
            // No `--stage` named: the unblocker's pass is taken at its word
            // for `origin` itself, so `origin`'s own gate is what answers for
            // it — not the step `cleared_block_target` would carry it to,
            // which has done no work yet for any gate to hold. Read before
            // `cleared_block_target` runs: that call reads `blocked_from` too,
            // through the same `resume_target`, and nothing between here and
            // there changes what it would answer.
            //
            // Only an agent step is held this way. A command step has no work
            // for the unblocker to stand in for, so `cleared_block_target`
            // hands it back to itself; holding it here would let `resume` read
            // the pause as a caught pass and send a command that never exited
            // 0 down its `on_pass`.
            None if pipeline.step(&origin).is_some_and(|step| {
                step.gate && step.kind() == crate::pipeline::StepKind::Agent
            }) =>
            {
                task.front.paused_at = Some(origin.clone());
                task.front.paused_by = Some(Gate::Step.as_str().to_string());
                task.front.blocked_from = None;
                gated_at = Some(origin.clone());
                crate::pipeline::PAUSED.to_string()
            }
            None => cleared_block_target(task, pipeline, true),
        };
        // The unblocker's destination is a move like any other, so it lands
        // past the steps this task does not run.
        let landing = crate::dispatch::land_past_hidden(pipeline, task, target, dependents);
        walked_past = landing.note();
        let target = landing.destination;
        // Leaving `blocked` starts every `loop:` count again, ahead of the
        // budget check below: a spent limit is what sent the task here, and
        // the unblocker's pass is its answer, so the step it lands on must
        // not be refused for the arrivals that got it stopped. Spoolway's own
        // counters never park a task — only the unblocker's judgement does.
        task.reset_loop_counts();
        let routed_target = apply_loop_budget(pipeline, task, current, target.clone(), unattended);
        if gated_at.is_none() && routed_target == target {
            resume_at(task, &target);
        }
        routed_target
    } else if paused_from_blocked {
        // No second destination once the lane on `blocked` cannot clear the
        // task itself. `blocked` declares no `on_fail`, and `Outcome::Block`
        // (and now `Outcome::Pause`) always resolve to `blocked` by
        // definition — which is the loop this whole task exists to close, and
        // an unbounded one: the archive holds a run where four tasks looped
        // there twelve, seven, six and two times.
        //
        // So this parks on `paused` instead, exactly as a gated pass does,
        // and — unlike an ordinary block, which calls `set_blocked_from`
        // below — leaves `blocked_from` untouched rather than clearing it.
        // `spoolway resume` needs it later to know which step to hand the
        // task back to — through the same `cleared_block_target` above, but
        // with `takes_over: false`, since nothing here claimed that step's
        // work was done; clearing `blocked_from` here would leave that
        // resume nothing to read and no better fallback than the pipeline's
        // entry.
        //
        // Parking is still leaving `blocked`, so every `loop:` count starts
        // again here too: the person's resume from `paused` must not walk the
        // task straight back into the limit that sent it to `blocked`.
        task.reset_loop_counts();
        let origin = task
            .front
            .blocked_from
            .clone()
            .unwrap_or_else(|| resume_target(task, pipeline));
        task.front.paused_at = Some(origin);
        crate::pipeline::PAUSED.to_string()
    } else {
        let mut landing = crate::dispatch::land_past_hidden(
            pipeline,
            task,
            step.destination(outcome)
                .unwrap_or(crate::pipeline::BLOCKED)
                .to_string(),
            dependents,
        );
        spend_walked_past(pipeline, task, &mut landing, current, unattended);
        walked_past = landing.note();
        landing.destination
    };

    destination = apply_loop_budget(pipeline, task, current, destination, unattended);

    // Record the step it stopped on, exactly as the dispatcher's own escalation
    // does, whatever put it there — an explicit `--block`, an `on_fail` that
    // routes to `blocked`, or the spent budget above. Both roads out of here
    // read it: `spoolway resume` resumes from it, and so does the unattended
    // resume below. Without it a task restarts at the pipeline's entry, and one
    // that blocked at `pr` already has a branch pushed and a pull request open,
    // so re-running the steps that did that opens a second one.
    if destination == crate::pipeline::BLOCKED {
        set_blocked_from(task, current);
    }

    // A gated step's pass is not spoolway's to act on. The work is done and it
    // went well; whether the task goes past this step is the person's, which is
    // the whole of what `gate:` says — so the task lands on `paused` and waits
    // for `spoolway resume`.
    //
    // Held here rather than asked of the lane. The lane is told that a person
    // will read its pane — see `dispatch::policy` and `dispatch::situating` —
    // but told nothing it could act on to change this decision: it cannot
    // approve its own work, so the only thing knowing changes is what it
    // leaves running and what it writes for that person, not where the pass
    // parks. The old arrangement asked it to stop and print a question
    // instead, and that request was the entire enforcement — three plan runs
    // against a small local model, three gates, and not one of them held. A
    // mechanism that needs no cooperation cannot be argued with.
    //
    // `gate: true` only ever means this: a pass. A `--fail` goes round the loop
    // the pipeline drew, which is not the thing a gate is protecting, and a
    // `--block` already stops in front of a person with the reason attached —
    // turning that into an approval would throw the blocker away and offer to
    // let unfinished work past instead. A task's own `gate_at` — the `if
    // scheduled` branch below — answers a different question and holds
    // whatever this step reports: a person reaching for it mid-turn wants to
    // see what actually came back, not only a pass.
    //
    // Holds in an unattended run too. `unattended` skips the checks that only
    // exist to catch a *lane* going wrong without a person to escalate to —
    // the launch ceiling becomes a backoff, and a `blocked` with nobody
    // staffing it resumes the lane instead of parking. A gate is not one of
    // those: it is a person's decision by design, and a run with nobody in it
    // is not a reason to make that decision unattended — it is a reason to
    // wait longer for the person who will.
    // Two ways a step earns this, not one: the pipeline's own `gate: true`,
    // which holds every task that reaches the step and only ever catches its
    // pass, and a task's own `gate_at`, set by whoever wrote its task to
    // hold this one task without giving it a pipeline of its own — and which
    // catches this step's outcome whatever it was, `destination` already
    // carrying whatever `apply_loop_budget` and the `blocked` check above made
    // of it. `spoolway resume` is what tells a caught pass from a caught fail
    // or block apart again, from `last_report` and `blocked_from` — see
    // `resume_road`.
    let hold = gate_hold(task, step, outcome, &destination);
    let gated = hold.is_some() || gated_at.is_some();
    // What the status log's arrival line says, in place of the lane's own
    // `-m` message — the Mockup draws this note on the arrival, not the
    // lane's own account of its pass, which `report` writes back in as a
    // second line credited to `current` once `set_stage` has banked this
    // one. A lane leaving something for the person who answers the gate to
    // read still has `--handoff` for it too, credited to `current` in
    // `## Handoff` above, same as any other step.
    let mut pause_note = None;
    if let Some(kind) = hold {
        task.front.paused_at = Some(current.to_string());
        task.front.paused_by = Some(kind.as_str().to_string());
        destination = crate::pipeline::PAUSED.to_string();
        // Held where it is: the resume that lets it go lands past hidden
        // steps itself, so nothing was walked past here.
        walked_past = None;
        pause_note = Some(match kind {
            Gate::Schedule => "held by this task's own schedule".to_string(),
            Gate::Step => "held by this step's own gate".to_string(),
        });
        // Spent, not standing, whoever wrote it — the board's `s` is the
        // example, but a `gate_at` typed by hand into the task fires and
        // clears exactly the same way. A step's own `gate: true` is the one
        // that holds every task that ever reaches it. Left set, a later
        // route that brought this task back onto the same step — a loop, a
        // `--stage` reroute — would gate it a second time nobody asked for.
        if kind == Gate::Schedule {
            task.front.gate_at = None;
        }
    }
    // The origin-gate hold above already stamped `paused_at`/`paused_by`
    // itself, naming the step this pass stands in for rather than `blocked`
    // — `hold` never fires for it (`step` here is `blocked`'s own, and
    // `blocked` is never gated), so the note is written here instead, next
    // to the one `hold` would have written for an ordinary gate.
    if let Some(origin) = &gated_at {
        pause_note = Some(format!("held by `{origin}`'s gate"));
    }

    // And in an unattended run, that is as far towards `blocked` as it gets.
    // There is nobody to park in front of, so the task goes back to the step it
    // stopped on to have another go — the same lane, continued, with the
    // blocker it wrote sitting in its own `## Blocker` for it to read.
    //
    // *Why* it stopped is deliberately not consulted. An explicit `--block`, a
    // fail that fell through, a spent round limit: all three say this task is
    // not moving without help, and in a run with nobody in it the only help
    // there is is another go by the lane that knows what happened.
    //
    // A gated *pass* never reaches here: it is turned into `paused` above,
    // before `destination` can equal `blocked`, and unattended does not
    // change that — see `unattended.enabled`. A `--block` from a step whose
    // only gate is `gate: true` is a different report, though, and does
    // reach this resume like any other block; a `gate: true` only holds the
    // work it approves, not the work that stopped short of it. A task's own
    // `gate_at` is not that step: it holds a `--block` the same as a pass, so
    // one caught by a schedule never reaches this resume at all — see the
    // `if scheduled` branch above.
    // Skipped whenever the pipeline stages `blocked` and staffs it in this
    // run — see `Pipeline::blocked_is_staffed`. There, going to `blocked`
    // starts an ordinary lane on it rather than resuming this one.
    let mut resumed = None;
    if unattended
        && destination == crate::pipeline::BLOCKED
        && !pipeline.blocked_is_staffed(unattended)
    {
        let target = resume_target(task, pipeline);
        // The task never sits on `blocked` in this configuration, so nothing
        // resets the counts here and the budgets stay spent. A spent budget
        // never arrives here in the first place: `apply_loop_budget` skips a
        // limit whose exit is `blocked` in exactly this configuration, rather
        // than handing the task a wall it can only walk into again.
        resume_at(task, &target);
        let landing = crate::dispatch::land_past_hidden(pipeline, task, target, dependents);
        walked_past = landing.note();
        destination = landing.destination.clone();
        resumed = Some(landing.destination);
    }

    Ok(Routed {
        destination,
        gated,
        pause_note,
        resumed,
        paused_from_blocked,
        gated_at,
        walked_past,
    })
}

/// An arrival that would spend a step's `loop:` past its limit escalates
/// instead of landing there, rather than a task landing on `destination`
/// unconditionally. Counting is what makes this a lookup rather than a
/// judgement call about whether progress is being made — and any lane's move
/// counts, a pass included, so a lane cannot read its way past a limit by
/// reporting the outcome that was never refused before.
///
/// The budget belongs to the step arrived at, not the one sending the task
/// there: `loop: 2` on `fix` is read here as "`fix` may arrive at most twice,
/// by any route" — the same count [`Task::rounds_at`] banks and the board
/// draws as `↻`. Bounding the sender's own route instead — which is what this
/// did — let a step reached from more than one place run once per sender
/// times its own limit, and let a hand `spoolway resume --stage` reach a
/// spent step by a route no counter was watching.
///
/// Every move that lands a task on a step goes through here, so no road
/// round the limit is left. [`report`] asks it for a lane's own account of
/// itself, and the dispatcher asks it for the other four: a `run:` step's
/// exit code (a mechanical gate failing back to the agent step behind it is
/// exactly that case), a task found sitting on a step walked past by `skip:`,
/// `first:` or `last:`, a lane that could not be started or whose pane never
/// came free, and a background command that failed after its task had moved
/// on. A walk-past is counted at the step it lands on; the `loop:` of a
/// step it skips is spent by [`walked_past_budgets`] before this is asked.
///
/// What it counts is laps, not conversations: a step with `session: true` may
/// be re-prompted as often as its session survives, with an optional separate
/// bound — `session_reuse_ctx` on the agent profile.
///
/// The exit is `blocked`, almost always — a loop that will not converge is a
/// request for a person, and a pipeline no longer gets to say otherwise. A
/// report made from `blocked` itself never arrives with a spent count: every
/// road out of it resets them first (see [`Task::reset_loop_counts`]), so this
/// never has to park a task on `paused` for a counter's sake.
///
/// And an unattended run with nobody staffing `blocked` has nothing to park
/// the task in front of: the run answers that exit itself, sending the task
/// straight back to the step it stopped on, and the budget would then be
/// spent again on the very next transition, and every one after it, with
/// nobody to clear it — the same loop, one lane more expensive per lap. The
/// bound is skipped outright in that one configuration, so the task file does
/// not fill with arrivals bought by a wall the run can only walk into.
/// [`resume_at`] refunds nothing itself, but a task leaving `blocked` has its
/// counts reset before this is asked, so this is the whole of the carve-out.
pub fn apply_loop_budget(
    pipeline: &Pipeline,
    task: &mut Task,
    current: &str,
    destination: String,
    unattended: bool,
) -> String {
    let Some(dest_step) = pipeline.step(&destination) else {
        return destination;
    };
    let Some((limit, count)) = spent_budget(pipeline, task, dest_step, unattended) else {
        return destination;
    };

    // The move it is not making, counted the way a reader counts: the budget
    // is spent, so the one being refused is the next arrival after it.
    let exit = dest_step.loop_exit();
    task.log_status(&format!(
        "`{current}` may not send this to `{destination}` a {} time — `{destination}` has \
         `loop: {limit}`; carrying on to `{exit}`",
        ordinal(count + 1)
    ));
    exit.to_string()
}

/// The `loop:` limit and arrival count of `step` when its budget is spent,
/// or `None` when it has no budget or has room left. The one test
/// [`apply_loop_budget`] and [`walked_past_budgets`] both ask, so the
/// unattended carve-out cannot drift between them.
fn spent_budget(
    pipeline: &Pipeline,
    task: &Task,
    step: &Step,
    unattended: bool,
) -> Option<(u32, u32)> {
    let limit = step
        .arrival_limit()
        .filter(|_| !unattended || pipeline.blocked_is_staffed(unattended))?;
    let count = task.rounds_at(&step.id);
    (count >= limit).then_some((limit, count))
}

/// Cut a move short at the first step it walked past whose `loop:` is spent,
/// and answer the steps it still passes that carry a `loop:` — the arrivals
/// [`bank_walked_past`] must count once the move is actually written.
///
/// [`apply_loop_budget`] only ever sees the step a move lands on. A cycle
/// whose one `loop:` sits on a step the task walks past (`skip:`, `first:`,
/// `last:`) then has no counter anywhere: the task never arrives at the
/// bounded step, so nothing stops it going round for ever, one full lane per
/// lap. A pass over such a step stands in for the arrival it never made, and
/// the pass past a spent one lands on its [`Step::loop_exit`] instead.
///
/// This counts no arrival: that is [`bank_walked_past`]'s, because a
/// move onto an agent step is not written until its lane starts, and a start
/// that is refused leaves the old stage on disk to be routed again: an
/// arrival banked now would be counted once per attempt.
///
/// Steps without a `loop:` are left uncounted, so a walk-past still leaves
/// no arrival on a step that has no budget to spend.
pub(crate) fn walked_past_budgets(
    pipeline: &Pipeline,
    task: &mut Task,
    landing: &mut crate::dispatch::Landing,
    current: &str,
    unattended: bool,
) -> Vec<String> {
    let mut banked = Vec::new();
    for index in 0..landing.passed.len() {
        let id = landing.passed[index].0.clone();
        let Some(step) = pipeline.step(&id) else {
            continue;
        };
        if step.arrival_limit().is_none() {
            continue;
        }
        if let Some((limit, count)) = spent_budget(pipeline, task, step, unattended) {
            let exit = step.loop_exit().to_string();
            task.log_status(&format!(
                "`{current}` may not send this past `{id}` a {} time — `{id}` has \
                 `loop: {limit}`; carrying on to `{exit}`",
                ordinal(count + 1)
            ));
            landing.passed.truncate(index);
            landing.destination = exit;
            return banked;
        }
        if !unattended || pipeline.blocked_is_staffed(unattended) {
            banked.push(id);
        }
    }
    banked
}

/// Count one arrival on each of `ids`, in the map [`apply_loop_budget`]
/// reads. Called where the move's stage is written.
pub(crate) fn bank_walked_past(task: &mut Task, ids: &[String]) {
    for id in ids {
        *task.front.arrivals.entry(id.clone()).or_insert(0) += 1;
    }
}

/// [`walked_past_budgets`] and [`bank_walked_past`] in one, for the moves
/// that write their stage in the same call.
pub(crate) fn spend_walked_past(
    pipeline: &Pipeline,
    task: &mut Task,
    landing: &mut crate::dispatch::Landing,
    current: &str,
    unattended: bool,
) {
    let ids = walked_past_budgets(pipeline, task, landing, current, unattended);
    bank_walked_past(task, &ids);
}

/// `3` → `3rd`, for the one sentence that counts the move a spent budget is
/// refusing. The teens are the carve-out every English ordinal has: 11, 12
/// and 13 take `th` however they end.
fn ordinal(n: u32) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Record where a task stopped, guarded so a block reported *from* `blocked`
/// itself never overwrites the step it originally stopped on.
///
/// Two writers now agree on this: `spoolway report`'s ordinary route to
/// `blocked`, and the dispatcher's own `escalate`. A watchdog killing a
/// staffed `blocked` lane reports (or is escalated) with `current ==
/// blocked`, and without the guard that
/// overwrites `blocked_from` with `blocked` — the one place `resume_target`
/// would then never find its way back from.
pub fn set_blocked_from(task: &mut Task, current: &str) {
    if current != crate::pipeline::BLOCKED {
        task.front.blocked_from = Some(current.to_string());
    }
}

/// Commit whatever the lane left uncommitted in its worktree, and say what had
/// to be picked up.
///
/// Only ever inside a lane: `SPOOLWAY_WORKTREE` is set by the dispatcher when it
/// starts one and by nothing else, so a person running `spoolway report` by hand
/// in their own checkout never has their working tree swept into a commit.
///
/// `None` when there is no lane worktree to act on. Otherwise the [`AutoCommit`]
/// says what happened.
fn commit_lane_work(repo: &Repo, task: &str, step: &str) -> Option<AutoCommit> {
    let worktree = std::path::PathBuf::from(crate::platform::env_var("SPOOLWAY_WORKTREE").ok()?);
    let head = crate::platform::env_var("SPOOLWAY_HEAD").unwrap_or_default();
    Some(auto_commit(repo, &worktree, &head, task, step))
}

/// What [`auto_commit`] did, or could not do — the difference a caller about to
/// tear a worktree down has to be able to tell.
#[derive(Debug)]
pub enum AutoCommit {
    /// The tree was clean. Nothing to record, nothing to say.
    Clean,
    /// The lane's leftovers are now in a `wip(...)` commit on its branch.
    Committed(String),
    /// Leftovers remain, and this lane recorded no work of its own: a git
    /// command failed, or `dispatch.auto_commit` is off. Tearing the worktree
    /// down now destroys this — see [`AutoCommit::is_unrecorded`].
    Unrecorded(String),
    /// Leftovers remain, but this lane did commit its own work — these are
    /// residue (a generated lockfile, a build artefact no `.gitignore`
    /// covers). Left uncommitted on purpose: sweeping it into a commit the
    /// lane did not intend gets the change failed in review for touching
    /// files the task's non-goals forbid. It is named in the status log and
    /// backed up nowhere; a later cleanup knowingly discards it rather than
    /// turning residue into task work.
    Residue(String),
}

impl AutoCommit {
    /// The line for `## Status Log`, if there is anything worth saying.
    pub fn note(&self) -> Option<&str> {
        match self {
            AutoCommit::Clean => None,
            AutoCommit::Committed(s) | AutoCommit::Unrecorded(s) | AutoCommit::Residue(s) => {
                Some(s)
            }
        }
    }

    /// Whether uncommitted work must prevent the worktree from being torn down.
    /// `Residue` is not this: it is named in the status log but backed up
    /// nowhere, and cleanup knowingly discards it because blocking for residue
    /// would strand every task that leaves a lockfile behind.
    pub fn is_unrecorded(&self) -> bool {
        matches!(self, AutoCommit::Unrecorded(_))
    }
}

/// The one git verb spoolway runs of its own accord: commit the lane's worktree
/// when its step settles, so that reaching `done` cannot delete work
/// that was never recorded anywhere.
///
/// Called from both ends of a step — `spoolway report`, which is how a lane that
/// finished settles, and the dispatcher's escalation and cleanup, which is how
/// one that died mid-run does. Both need the same behaviour, and a lane that
/// dies is the case where the work is least likely to have been committed by
/// hand.
///
/// `git add -A` honours `.gitignore`, so properly-ignored build output stays
/// out; anything else a step leaves behind is committed and visible in the diff,
/// which is a review question rather than something to add a setting for.
///
/// `started_at` is where the branch stood when the lane began; an empty one means
/// nobody recorded it, and the backstop then commits, because "I do not know"
/// must not mean "throw the work away".
///
/// The [`AutoCommit`] return is what a caller about to tear the worktree down
/// reads: only [`AutoCommit::Unrecorded`] is work that would be lost, and
/// [`report`] and [`crate::dispatch::Dispatcher::clean_up`] both hold the task
/// at `blocked` rather than clean up when they see it.
pub fn auto_commit(
    repo: &Repo,
    worktree: &std::path::Path,
    started_at: &str,
    task: &str,
    step: &str,
) -> AutoCommit {
    // Not a git worktree at all — an empty or already-gone borrowed checkout,
    // say. There is nothing git-tracked here to lose, so this is `Clean`, not
    // a git failure. The check below is for a real worktree that git then
    // refuses to act on.
    if !worktree.is_dir() || !worktree.join(".git").exists() {
        return AutoCommit::Clean;
    }

    // A git that will not answer is not a clean tree. Swallowed as "nothing to
    // commit" — the way `.ok()?` did — it means no status-log line, and a
    // cleanup terminal then deletes a worktree whose work was never recorded
    // (review finding 4). So a failure here is `Unrecorded`, not `Clean`.
    let dirty = match crate::repo::run(worktree, "git", &["status", "--porcelain"]) {
        Ok(out) => out,
        Err(e) => {
            return AutoCommit::Unrecorded(format!(
                "could not check `{step}`'s worktree for uncommitted work: {e}"
            ));
        }
    };
    let files = dirty.lines().filter(|l| !l.trim().is_empty()).count();
    if files == 0 {
        return AutoCommit::Clean;
    }

    // A real lane's `SPOOLWAY_TASK` names the task it was actually started
    // for, and `SPOOLWAY_WORKTREE` names its own worktree — the same one
    // `commit_lane_work` reads `worktree` from, and the same one `spoolway
    // stack` reads it from at `handover`. So whenever the worktree this call
    // was actually handed *is* that lane's own — the ordinary case, on every
    // real call — a `task` that disagrees with `SPOOLWAY_TASK` means this
    // call did not come from that lane's own turn at all. It came from this
    // module's own tests: `cargo test` runs inside a lane's real worktree, so
    // both variables are already set to *that* lane's own task when the suite
    // starts, and a test calling `report()` or `auto_commit` against that
    // same worktree with a fixture id commits into it under the real lane's
    // name instead of the fixture's. Seen for real: a run's own suite
    // committed two other lanes' branches as `wip(stuck): work` and
    // `wip(login): implement`.
    //
    // The worktree comparison is what keeps this from also catching the
    // dispatcher's own direct calls (`src/dispatch.rs`, tearing down a dead
    // lane or committing a person's round in a held pane): those name a
    // worktree out of the task file itself, which only happens to equal
    // `SPOOLWAY_WORKTREE` when the dispatcher's own process was started
    // inside a lane's shell — not the ordinary case, and not one this guard
    // needs to answer for.
    if let Ok(env_task) = crate::platform::env_var(crate::commands::TASK_ENV)
        && env_task != task
        && crate::platform::env_var("SPOOLWAY_WORKTREE")
            .is_ok_and(|w| std::path::Path::new(&w) == worktree)
    {
        return AutoCommit::Unrecorded(format!(
            "left {files} uncommitted file(s) behind — not committed, because this process's \
             `{}` names `{env_task}`, not `{task}`",
            crate::commands::TASK_ENV,
        ));
    }

    if !repo.config.dispatch.auto_commit {
        // Residue or lost work is still a real question with the backstop
        // off — the one place this answer is read is a cleanup about to
        // remove the worktree. A tracked file modified or deleted is work
        // a person would have to redo; a tree whose only leftovers are
        // untracked — a lockfile, a `target/`, a generated file — is
        // residue, and holding every such task at `blocked` (as 0.2.0's
        // first cut did) sent each one back round its last agent step and
        // `handover` for nothing. 0.1.0 archived these; so does this.
        let tracked = dirty
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with("??"))
            .count();
        if tracked == 0 {
            return AutoCommit::Residue(format!(
                "left {files} untracked file(s) behind — not committed, because \
                 `dispatch.auto_commit` is off, and nothing tracked was changed"
            ));
        }
        return AutoCommit::Unrecorded(format!(
            "left {files} uncommitted file(s) behind, {tracked} of them tracked — not \
             committed, because `dispatch.auto_commit` is off"
        ));
    }

    // Did this lane commit anything of its own? A HEAD that has moved since the
    // lane started means it did.
    //
    // This is the difference between a backstop and a nuisance. A lane that
    // committed nothing has its whole turn's work here, and losing it would be
    // the real failure. A lane that committed properly has left *residue* —
    // a `Cargo.lock` a test run generated, an artefact no `.gitignore` covers —
    // and sweeping that into a commit the lane did not intend gets the change
    // failed in review for touching files the task's non-goals forbid, which is
    // a loop nothing downstream can break. Observed doing exactly that.
    //
    // Residue is named in the status log and backed up nowhere. It comes back
    // as [`AutoCommit::Residue`], not `Unrecorded`, so cleanup knowingly
    // discards it rather than committing files the lane excluded; holding every
    // task that leaves a lockfile behind would be its own loop.
    let now = match crate::repo::run(worktree, "git", &["rev-parse", "HEAD"]) {
        Ok(out) => out,
        Err(e) => {
            return AutoCommit::Unrecorded(format!(
                "could not commit {files} file(s) the `{step}` step left — \
                 `git rev-parse HEAD` failed: {e}"
            ));
        }
    };
    if lane_committed(started_at, now.trim()) {
        return AutoCommit::Residue(format!(
            "left {files} uncommitted file(s) behind — not committed, because `{step}` \
             made its own commits and these were not among them"
        ));
    }

    if let Err(e) = crate::repo::run(worktree, "git", &["add", "-A"]) {
        return AutoCommit::Unrecorded(format!(
            "could not commit {files} file(s) the `{step}` step left: {e}"
        ));
    }
    // `wip(<task>): <step>` — shaped so that a reader scanning a stacked pull
    // request can tell spoolway's backstop commits from the ones a lane wrote on
    // purpose, and so `spoolway stack` can squash them without reading each one.
    let message = format!("wip({task}): {step}");
    if let Err(e) = crate::repo::run(worktree, "git", &["commit", "-q", "-m", &message]) {
        return AutoCommit::Unrecorded(format!(
            "could not commit {files} file(s) the `{step}` step left: {e}"
        ));
    }

    AutoCommit::Committed(format!(
        "committed {files} file(s) the `{step}` step left uncommitted"
    ))
}

/// Whether the lane made commits of its own during its turn.
///
/// An empty `started_at` means nobody recorded where the branch stood — a lane
/// from a spoolway that predates the field, or one started outside a pass. The
/// backstop then behaves as it always did and commits, because "I do not know"
/// must not mean "throw the work away".
fn lane_committed(started_at: &str, head_now: &str) -> bool {
    !started_at.is_empty() && started_at != head_now
}

/// Where a task that stopped goes back to: the step it stopped on, or the
/// nearest thing to it anything still remembers.
///
/// Starting a half-finished task over is not the safe choice it looks like: a
/// task that blocked at `handover` already has a branch pushed and a pull
/// request open, and re-running the steps that did that opens a second one.
///
/// `blocked_from` is the real answer and is usually there. When it is not —
/// cleared by an earlier [`resume_at`], or naming a step the pipeline no longer
/// has — this used to fall straight through to the pipeline's entry, and that
/// was the single most expensive line in the file. Three of four tasks that hit
/// `blocked` in one run came back at `implement` and re-walked the whole
/// pipeline, one of them for about seven agent turns, over a suite run that had
/// been killed rather than failed.
///
/// So `last_report` is tried in between. It is a weaker signal — a lane that
/// died without reporting leaves it pointing at the step *before* the one that
/// matters — but being one step early is a different order of wrong from being
/// at the entry, and the entry is now only reached when a task has never
/// reported at all.
///
/// Neither candidate may be `blocked` itself. [`set_blocked_from`] already
/// refuses to write it for the reason its own comment gives — it is the one
/// origin there is no way back from — but `last_report` is written by whatever
/// last reported, and a staffed `blocked` step reports like any other. Once its
/// lane has been round once, an absent `blocked_from` would otherwise resolve to
/// `blocked`, whose `on_pass` is `None`, so [`cleared_block_target`] hands back
/// `blocked` again and every further pass loops there. The fallback is worth
/// nothing in exactly the case it exists for, so it skips `blocked` and lets the
/// entry have it.
pub fn resume_target(task: &Task, pipeline: &Pipeline) -> String {
    // A task that never started — parked off `queued` by the board's `p`
    // or `spoolway queue pause`, or held on `paused` by the tracking hook —
    // has no step to go back to, and the entry is the wrong answer: it
    // skips the dependency and hook gates only the `queued` arm applies, so
    // a resumed task would be launched off a dependency still mid-work.
    // Back onto `queued`, where it is gated like any other (jobs review
    // finding 1). Nothing else leaves all four unset: a launch that failed
    // records `blocked_from`, a lane that ran records a checkout.
    if task.front.blocked_from.is_none()
        && task.front.last_report.is_none()
        && task.front.worktree_path.is_none()
        && task.front.workspace_id.is_none()
    {
        return crate::pipeline::QUEUED.to_string();
    }
    let known = |step: Option<&str>| {
        step.filter(|id| *id != crate::pipeline::BLOCKED)
            .filter(|id| pipeline.step(id).is_some())
            .map(str::to_string)
    };
    known(task.front.blocked_from.as_deref())
        .or_else(|| known(task.front.last_report.as_ref().map(|r| r.step.as_str())))
        .unwrap_or_else(|| pipeline.entry().to_string())
}

/// Whether `id` is a reserved stage rather than a step of any pipeline, so a
/// pipeline lacking it has lost nothing. It is the same set `pipeline check`
/// skips. `started` is not in it: a pipeline may name a step that, and a task
/// recorded on one that was then renamed is as stranded as any other.
fn is_stage_word(id: &str) -> bool {
    crate::pipeline::RESERVED.contains(&id)
}

/// The step a task recorded that its pipeline no longer has, when a plain
/// resume of it would otherwise land somewhere wrong.
///
/// A task standing on a step the pipeline renamed away has nothing to go
/// back to: its `stage:` is the step itself, and sending it to the entry or
/// to `last_report` would redo passed work or run a step early. A task
/// stopped by a block is the same when its `blocked_from` and `last_report`
/// both name steps the pipeline dropped, since [`resume_target`] would then
/// answer with the entry. `blocked` is skipped for the reason
/// [`resume_target`] gives. A recorded step that does still exist means
/// [`resume_target`] has a real answer, and nothing is stranded. A task that
/// never reported records nothing and rightly resumes at the entry.
pub fn stranded_step(task: &Task, pipeline: &Pipeline) -> Option<String> {
    let stage = task.stage();
    if !is_stage_word(stage) && pipeline.step(stage).is_none() {
        return Some(stage.to_string());
    }
    let recorded = [
        task.front.blocked_from.as_deref(),
        task.front.last_report.as_ref().map(|r| r.step.as_str()),
    ];
    let recorded = recorded
        .into_iter()
        .flatten()
        .filter(|id| *id != crate::pipeline::BLOCKED);
    let mut missing = None;
    for id in recorded {
        if pipeline.step(id).is_some() {
            return None;
        }
        missing.get_or_insert(id);
    }
    missing.map(str::to_string)
}

/// The refusal for a plain resume of a task whose recorded step `pipeline`
/// no longer has: it names the step and `--stage`, the way to say where the
/// task goes instead of guessing.
fn missing_step_refusal(task: &Task, pipeline: &Pipeline, missing: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "task `{id}` stopped at `{missing}`, which pipeline `{pipeline}` no longer defines, so a \
         plain resume has no step to send it back to. Name one with `spoolway resume {id} \
         --stage <step>`.",
        id = task.front.id,
        pipeline = pipeline.name
    )
}

/// Which of the two roads holds a task at `step` for `outcome`/`destination`
/// — the one predicate every reader of a gate now shares, rather than each
/// deriving its own copy. `compose::report_contract` asks it of a
/// hypothetical pass, before a lane has run; [`report`] asks it of the real
/// outcome and the destination it just resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A task's own `gate_at`, set by whoever wrote its task — catches
    /// this step's outcome whatever it was.
    Schedule,
    /// A step's own `gate: true` — catches a pass and only a pass, and only
    /// one whose destination is not already `blocked`.
    Step,
}

impl Gate {
    /// The word this is recorded as in `paused_by` — see [`crate::task::
    /// Frontmatter::paused_by`].
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::Schedule => "schedule",
            Gate::Step => "gate",
        }
    }
}

/// Whether `step` holds this task for `outcome`/`destination` — the two
/// roads [`report`] parks a pass in front of a person for, and the
/// dispatcher parks a command step's exit for, carrying
/// both clauses a hand copy of this once dropped: a step's own `gate: true`
/// only ever catches a pass whose destination is not already `blocked`,
/// where a task's own `gate_at` catches whatever is reported. `Schedule`
/// wins the ambiguity when a task carries both, since it answers a broader
/// question and is spent the moment it fires either way — see [`report`]'s
/// own use of this.
pub fn gate_hold(task: &Task, step: &Step, outcome: Outcome, destination: &str) -> Option<Gate> {
    if task.front.gate_at.as_deref() == Some(step.id.as_str()) {
        return Some(Gate::Schedule);
    }
    if step.gate && outcome == Outcome::Pass && destination != crate::pipeline::BLOCKED {
        return Some(Gate::Step);
    }
    None
}

/// The first gated step in the pipeline's own order, at or after `origin` —
/// what an unblocker's `--pass` may not be carried past, whether by the
/// ordinary `on_pass` road or by a `--stage` naming a step further on. `None`
/// when `origin` no longer names a step this pipeline has, or nothing from
/// there on is gated.
///
/// Used directly by `compose::report_contract`, which names it in the
/// `--stage` form's own "never one past" clause before a lane has reported
/// anything at all, and indirectly by [`route`]'s own `--stage` refusal,
/// through [`gate_between`], which is built on this.
pub fn first_gated_from(pipeline: &Pipeline, origin: &str) -> Option<String> {
    let idx = pipeline.steps.iter().position(|step| step.id == origin)?;
    pipeline.steps[idx..]
        .iter()
        .find(|step| step.gate)
        .map(|step| step.id.clone())
}

/// [`first_gated_from`], bounded above by `target` (exclusive) — `None` when
/// the first gated step at or after `origin` is `target` itself, lies at or
/// past it, or `target` is not forward of `origin` at all, so a `--stage`
/// naming an already-run step behind `origin`, or the gated step itself, is
/// never refused by this.
///
/// What [`route`] refuses a `--stage` for: naming a step this bounds is
/// naming one past a gate this task's pass has not answered for.
pub fn gate_between(pipeline: &Pipeline, origin: &str, target: &str) -> Option<String> {
    let target_idx = pipeline.steps.iter().position(|step| step.id == target)?;
    let origin_idx = pipeline.steps.iter().position(|step| step.id == origin)?;
    if target_idx <= origin_idx {
        return None;
    }
    let gate_step = first_gated_from(pipeline, origin)?;
    let gate_idx = pipeline
        .steps
        .iter()
        .position(|step| step.id == gate_step)?;
    (gate_idx < target_idx).then_some(gate_step)
}

/// The three choices a stop offers, key first and then the command that does
/// the same thing, matching the board's own key line — `resume`, open the
/// pane, or park it in place. Printed once, straight into the pane a report
/// just landed on `paused`, the only place this particular moment is ever
/// seen: nothing rereads a task's own status log for it.
///
/// `step` is the step this report just settled — the one `spoolway lane
/// --attach` opens, since the same lane's pane is what stays up once a
/// report lands the task on a stop.
fn stop_choices(id: &str, step: &str) -> String {
    format!(
        "\n\n  resume         [r]   spoolway resume {id}\n  \
         open the pane  [o]   spoolway lane '{}' --attach\n  \
         park it        [p]   spoolway queue pause {id}",
        crate::mux::lane_name(step, id)
    )
}

/// The steps this task has ever launched a lane or a command run at, in the
/// pipeline's own order — what a `--stage` naming a step the task has never
/// been at is bounded by, and what its refusal names back.
///
/// Read off `steps:` (see [`Task::steps_at`]), the launch record — not
/// `rounds`, which forgets a bound `spoolway resume` already gave back, and
/// not the task's current stage alone, which says nothing about a step it
/// visited earlier and has since left. Pipeline order rather than the map's
/// own key order so the answer reads as the shape of the run, not an
/// alphabetised list of steps that happen to share no relation to each other.
///
/// `blocked` itself is excluded, whatever `steps:` says. A staffed
/// `blocked` bank a launch under its own arrival route the instant an
/// unblocker's lane starts (`finish_launch_bookkeeping`, the same as any other step), so
/// `blocked` is in that record on every real run this flag exists for — the
/// one where a lane is actually sitting on `blocked` to type `--stage` at
/// all. Left in the bound, `--stage blocked` would be accepted:
/// `resume_at` clears `blocked_from`, and the next plain `--pass` from
/// `blocked` falls through `resume_target` all the way to the pipeline's
/// own entry, restarting a task that may already have pushed a branch or
/// opened a pull request. `blocked` is also not a destination a `--stage`
/// makes any sense naming — clearing a block by sending it back to the
/// block is not a target this flag exists to reach.
fn steps_run(task: &Task, pipeline: &Pipeline) -> Vec<String> {
    pipeline
        .steps
        .iter()
        .filter(|step| step.id != crate::pipeline::BLOCKED && task.steps_at(&step.id) > 0)
        .map(|step| step.id.clone())
        .collect()
}

/// Where clearing a block takes the task, which is not the same question as
/// where it stopped.
///
/// The unblocker prompt is told to do the blocked step's work — write the
/// code, fix the check, make the call, and say what it did. Taking that at its
/// word means its pass *is* that step's pass, so the task carries on from
/// wherever that step's `on_pass` pointed rather than arriving back at a step
/// whose work is already done. Handing it back costs a second full turn on the
/// same step to reach the same verdict.
///
/// **A command step is never carried past — it is handed back to itself.**
/// The unblocker's word is good for an agent step, whose whole output is the
/// claim it makes in its report. A command step's output is a `git push` or a
/// pull request opened, and "I did that step's work" from an unblocker does
/// not make either one exist; only running the command does. So a task
/// blocked on a command step resumes on that same step, whatever `takes_over`
/// says.
///
/// `takes_over` is the reported verb, not a setting: [`report`]'s own pass
/// from `blocked` passes `true` unconditionally, because a `--pass` is the
/// only outcome that reaches this function directly — the work is done, on
/// the unblocker's word. [`past_the_gate`] passes `false`, because reaching
/// it through a cleared block means a person is resuming a task that landed
/// on `paused` by `--pause`, `--fail` or `--block` — none of which claim the
/// step's work is done, so nothing here is carried past it.
///
/// Two cases hand back whatever `takes_over` says, because there is nothing to
/// carry the task to: an origin the pipeline no longer has, and an origin that
/// declares no `on_pass` of its own.
pub fn cleared_block_target(task: &Task, pipeline: &Pipeline, takes_over: bool) -> String {
    let origin = resume_target(task, pipeline);
    if !takes_over {
        return origin;
    }
    let Some(step) = pipeline.step(&origin) else {
        return origin;
    };
    if step.kind() != crate::pipeline::StepKind::Agent {
        return origin;
    }
    step.on_pass.clone().unwrap_or(origin)
}

/// Whether `gated`, a command step, holds a pass its own command never gave.
///
/// A command step's pause is filed by the dispatcher with `last_report` from
/// that step, after its exit code chose the route. A pause filed from any
/// other step is an unblocker's `--pass` from `blocked`, which an earlier
/// release held at the origin's gate whatever kind of step the origin was.
/// Nothing ran at the step after that pass, so letting it past down `on_pass`
/// would send on a command that never exited 0; such a task is handed back to
/// the step instead, to run again, which is where a pass from `blocked` takes
/// it now.
/// [`resume_road`] and the board's `(next)` row both ask this, so the road
/// and the row name the same step.
pub fn command_pass_handed_back(task: &Task, step: &crate::pipeline::Step, gated: &str) -> bool {
    step.kind() == crate::pipeline::StepKind::Command
        && task
            .front
            .last_report
            .as_ref()
            .is_some_and(|report| report.step != gated)
        && caught_at(task, gated).is_some()
}

/// What a pause is holding, once it is known to be a catch at all — see
/// [`caught_at`], which is what tells a catch apart from a pause raised from
/// `blocked` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caught {
    /// A plain pass, whatever gated it — a step's own `gate: true` or a
    /// schedule that happened to catch one. Resuming reads exactly as an
    /// ordinary gate always has.
    Pass,
    /// A fail a schedule caught before it could go round the loop the
    /// pipeline drew. Resuming an agent step's still takes `on_pass`: taking
    /// that verdict is why the pause was scheduled. A command step's takes
    /// its `on_fail`, the route its exit code chose.
    Fail,
    /// A destination that was already `blocked` before the gate stepped in —
    /// a `--block`, a step's own `on_fail: blocked`, or a spent loop's own
    /// exit. Resuming sends it to `blocked`, exactly where it would have
    /// landed unheld.
    Blocked,
}

/// What a paused task's schedule or gate caught, read back from `paused_by`
/// and `last_report` rather than guessed from the stage alone.
///
/// `None` for a pause `commands::report` raised from `blocked` itself: that
/// road's own report was filed *from* `blocked`, not from `gated`, and never
/// sets `paused_by` — see [`crate::task::Frontmatter::paused_by`], the key
/// that now records the road outright and is what decides the Some/None
/// answer once it is set. The dispatcher's own hold files `last_report` from
/// `gated` in the same save. The exception is the hold on an unblocker's
/// pass from `blocked` at a gated step: it sets `paused_by` with
/// `last_report` from `blocked`. `route` still makes it for a gated agent
/// step, and an earlier release made it for a command step too, which
/// [`command_pass_handed_back`] tells apart. A task already sitting on
/// `paused` from before `paused_by` existed carries none, so the fallback
/// this was built on outright — `last_report.step == gated`, filed only by a
/// road that actually caught something — still answers for it.
///
/// `last_report` still does the rest once a catch is confirmed: `paused_by`
/// says only *that* something was caught, not *what* — the Pass/Fail/Blocked
/// split below still reads the outcome it banked and `blocked_from`, exactly
/// as it always has.
///
/// [`Caught::Blocked`] stands for more than a raw `--block`: `blocked_from`
/// naming `gated` is what `report` leaves behind whenever the destination it
/// computed was already `blocked` before the gate caught it — see
/// `set_blocked_from` and `apply_loop_budget` in [`report`] — and that one
/// fact is all that survives to be read back here.
pub fn caught_at(task: &Task, gated: &str) -> Option<Caught> {
    let report = task.front.last_report.as_ref()?;
    let caught = task.front.paused_by.is_some() || report.step == gated;
    if !caught {
        return None;
    }
    let outcome = report.outcome.parse::<Outcome>().ok()?;
    if task.front.blocked_from.as_deref() == Some(gated) {
        return Some(Caught::Blocked);
    }
    Some(match outcome {
        Outcome::Fail => Caught::Fail,
        _ => Caught::Pass,
    })
}

/// Set a stopped task up to carry on from `target`.
///
/// The whole of what resuming *is*, in one place, because there are now four
/// callers who must agree to the letter: `spoolway resume` by hand, a pass
/// reported from `blocked`, a lane's own report in an unattended run, and the
/// dispatcher's own escalation in one. A version of this that drifted between
/// them would be a task that resumes and then stops again on the next
/// transition, or one whose second lane starts cold on work the first had
/// already finished.
///
/// Refunds nothing itself, on any of the four roads. A `loop:` limit is the
/// step's own count of every arrival it has taken — the same `↻` the board
/// draws — and a resume from a step that is not `blocked` has no lap to hand
/// back that would not also erase an arrival a lane genuinely made. The one
/// reset there is belongs to leaving `blocked` ([`Task::reset_loop_counts`]),
/// which the callers that leave it make before they get here: a spent limit's
/// exit is `blocked`, and once the unblocker or a person has answered it the
/// counts start again rather than walking the task into the same wall.
///
/// The lane that stopped is marked to be *continued* rather than replaced,
/// when the task is going back to where it actually stopped. Whatever was in
/// the way was outside that lane's control — that is what a block is — so its
/// work stands, and it is often work that was already finished when it stopped.
/// A task being sent somewhere else has been rerouted rather than resumed, and
/// the lane at the far end has nothing to say about why it stopped — which is
/// the case a take-over lands in every time, and correctly: the step it is
/// carried to has a prompt of its own that was never in the room.
pub fn resume_at(task: &mut Task, target: &str) {
    if task.front.blocked_from.as_deref() == Some(target) {
        task.front.resume = Some(target.to_string());
    }
    task.front.blocked_from = None;
}

/// Which road a bare `spoolway resume <task>` takes out of a stop, and the
/// stage it lands the task on — see [`resume_road`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeRoad {
    /// Waiting at a gate (`paused_at`): let past it, by [`past_the_gate`].
    Gate {
        /// The step the task paused at.
        gated: String,
        /// What the pause caught — see [`caught_at`].
        caught: Option<Caught>,
        /// A pause raised from `blocked` itself rather than a catch at
        /// `gated`, which resumes through [`cleared_block_target`], or a
        /// gated command step held on an unblocker's pass, which
        /// [`command_pass_handed_back`] sends straight back onto `gated`.
        cleared_block: bool,
        destination: String,
    },
    /// A `p` park (`parked_from`): back onto the step it never left, by
    /// [`unpark`].
    Unpark(String),
    /// A hook that failed on `done`: straight back to `done`.
    HookDone,
    /// A task that never started: back onto `queued`.
    Queued,
    /// The ordinary road: back onto the step [`resume_target`] names.
    Step(String),
}

impl ResumeRoad {
    /// The stage this road names. A gate's and the ordinary road's then land
    /// past any step the task walks past — see [`ResumeRoad::landing`].
    pub fn destination(&self) -> &str {
        match self {
            ResumeRoad::Gate { destination, .. } => destination,
            ResumeRoad::Unpark(step) | ResumeRoad::Step(step) => step,
            ResumeRoad::HookDone => crate::pipeline::DONE,
            ResumeRoad::Queued => crate::pipeline::QUEUED,
        }
    }

    /// The stage [`resume`] writes for this road, before any `loop:` check:
    /// a gate's and the ordinary road's [`ResumeRoad::destination`] landed
    /// past the steps `task` walks past, as `resume_checked` and
    /// `back_onto_its_step` land them. A park goes back onto its own step and
    /// the other two onto `done` or `queued`, none of which is moved.
    /// `dependents` is [`crate::dispatch::same_group_dependents`]'s count.
    pub(crate) fn landing(&self, pipeline: &Pipeline, task: &Task, dependents: usize) -> String {
        let destination = self.destination().to_string();
        match self {
            ResumeRoad::Gate { .. } | ResumeRoad::Step(_) => {
                crate::dispatch::land_past_hidden(pipeline, task, destination, dependents)
                    .destination
            }
            ResumeRoad::Unpark(_) | ResumeRoad::HookDone | ResumeRoad::Queued => destination,
        }
    }
}

/// Where a bare `spoolway resume <task>` — the board's `r` — sends `task`,
/// read without moving it.
///
/// [`resume`] acts on this and `spoolway queue route` prints it, so the two
/// cannot disagree about where a resume goes: a route that named one step
/// while the resume landed on another would have a person approve a gate on
/// the strength of a destination that was never the real one. `--stage` is
/// not a road here; naming a step by hand is a person overriding all of this.
///
/// The roads are tried in the order [`resume`] has always tried them: a gate,
/// then a park, then a hook pause on `done`, then [`resume_target`], whose
/// `queued` answer is a task that never started.
///
/// A task whose recorded step the pipeline no longer has, whether it stands
/// on it, is parked on it, is held at its gate or blocked on it, is refused
/// rather than sent to the entry, which would redo passed work, or put back
/// on a stage nothing can run. The message names the step and `--stage`, the
/// way to resume at a step by hand.
///
/// Where a gate goes is read out of `pipelines` at resume time, from the step
/// recorded in `paused_at`, rather than out of anything the pass wrote down.
/// `pipelines` is the graph the resuming command routes on: the running
/// dispatcher's own copy while one runs — see [`crate::pipeline_snapshot`] —
/// and the files otherwise. So a pipeline edited while a task sat on `paused`
/// routes it the edited way once no dispatcher is running, or once the
/// running one restarts; until then the edit waits, since a step it added is
/// one that dispatcher could never start.
pub fn resume_road(task: &Task, pipelines: &Pipelines) -> Result<ResumeRoad> {
    if let Some(gated) = task.front.paused_at.clone() {
        let pipeline = pipelines.for_task(task)?;
        let Some(step) = pipeline.step(&gated) else {
            return Err(missing_step_refusal(task, pipeline, &gated));
        };
        // What this pause actually caught — `None` for a pause raised from
        // `blocked` itself, which is not a catch of anything.
        let caught = caught_at(task, &gated);
        // A pause raised from `blocked` itself, told apart from an intercepted
        // catch at `gated` by `caught` above: both leave `blocked_from` naming
        // the very step `paused_at` does, but only `blocked`'s own road leaves
        // no report filed from `gated` to read. Its own pass never runs
        // `blocked`'s absent `on_pass` — it takes `blocked_from`'s, through the
        // same `cleared_block_target` a pass from `blocked` reads — so accepting
        // it here has to reach exactly there too, rather than the plain
        // `on_pass` below, which is what an ordinary gate means and is not what
        // a person clearing this one is answering.
        //
        // `handed_back` is the same answer for a task an earlier release held
        // at a gated command step on an unblocker's pass: it carries a catch,
        // but one `blocked` filed rather than the command, so it too goes
        // back onto the step to run, not down the `on_pass` of a command that
        // never exited 0. The block it clears is the same one, so it is
        // reported as a cleared block.
        let handed_back = command_pass_handed_back(task, step, &gated);
        let cleared_block = handed_back
            || (caught.is_none() && task.front.blocked_from.as_deref() == Some(gated.as_str()));
        let destination = if handed_back {
            gated.clone()
        } else if cleared_block {
            cleared_block_target(task, pipeline, false)
        } else if caught == Some(Caught::Blocked) {
            // What `set_blocked_from` already ran for on the way here — a
            // `--block`, a step's own `on_fail: blocked`, or a spent loop's
            // own exit — a plain `resume` sends exactly where it would have
            // landed unheld.
            crate::pipeline::BLOCKED.to_string()
        } else if caught == Some(Caught::Fail) && step.kind() == crate::pipeline::StepKind::Command
        {
            // A command step never reported: its exit code chose a route and
            // the dispatcher held the task before taking it, so letting it
            // past means taking that route. Sending a held failure down
            // `on_pass` would skip a red test or suite.
            step.destination(Outcome::Fail)
                .unwrap_or(crate::pipeline::BLOCKED)
                .to_string()
        } else {
            step.destination(Outcome::Pass)
                .unwrap_or(crate::pipeline::BLOCKED)
                .to_string()
        };
        return Ok(ResumeRoad::Gate {
            gated,
            caught,
            cleared_block,
            destination,
        });
    }
    // A `p` park is answered differently from a real stop: nothing was ever
    // in the way, so putting it back is not a lap — see `unpark`.
    if let Some(step) = &task.front.parked_from {
        // A park needs no pipeline to resume, so a pipeline that is gone
        // altogether is left alone here; one that lost this step would put
        // the task back on a stage nothing can run.
        if let Ok(pipeline) = pipelines.for_task(task)
            && !is_stage_word(step)
            && pipeline.step(step).is_none()
        {
            return Err(missing_step_refusal(task, pipeline, step));
        }
        return Ok(ResumeRoad::Unpark(step.clone()));
    }
    // `done` has no later step to carry the task past, and is not a stage
    // `resume_target` can name, so a hook pause there goes straight back.
    if task.front.hook_paused.as_deref() == Some(crate::pipeline::DONE) {
        return Ok(ResumeRoad::HookDone);
    }
    let pipeline = pipelines.for_task(task)?;
    if let Some(missing) = stranded_step(task, pipeline) {
        return Err(missing_step_refusal(task, pipeline, &missing));
    }
    let target = resume_target(task, pipeline);
    Ok(match target == crate::pipeline::QUEUED {
        true => ResumeRoad::Queued,
        false => ResumeRoad::Step(target),
    })
}

/// One verb over every road out of a stop, so the person does not have to
/// know which one a task is on before naming it.
///
/// `repo.task()` already reads `task.front.paused_at` before anything else
/// here does, so the fact that decides which body applies is known before
/// the person is asked for anything. A task carrying `paused_at` finished a
/// gated step and is waiting to be let past it; `--stage` is still honoured
/// there, since naming a step by hand is a person overriding the route on
/// purpose. Anything else resumes the step that stopped it — a real block,
/// through `back_onto_its_step`'s ordinary road, or a `p` park, through
/// `unpark` beside it.
///
/// `from_step` is the calling lane's own step, read from `SPOOLWAY_STEP` at
/// the CLI boundary the same way `report`'s own `started_for` is — `None`
/// for a person typing at their own shell. A lane on `blocked` is the one
/// exception to [`refuse_from_lane`]: clearing a block is often a question
/// about another stopped task, and `blocked` already reads three of them
/// through [`crate::compose::toolbox`]. Even there, `--stage` stays refused
/// — rerouting a task is a decision about what the work is for, not about
/// what is in its way — and a task waiting on a gate (`paused_at`) stays
/// refused whoever asks, so nothing a person was asked to approve can be
/// approved by a lane. A lane on `blocked` also may not resume its own task
/// — the one `SPOOLWAY_TASK` names — since that would answer its own block.
///
/// Whoever asks, a task that is not stopped is refused: one still `queued`, or
/// one standing on an agent or command step. And `--stage` is refused while a
/// task in `depends_on` has not finished, so a child cannot run ahead of its
/// parent. The board's `r` goes through [`resume_held_row`], which skips the
/// not-stopped refusal; `spoolway queue resume` calls this one first for a task
/// that is queued or has something running on its step.
pub fn resume(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &ResumeArgs,
    from_step: Option<&str>,
) -> Result<()> {
    resume_checked(repo, pipelines, args, from_step, false)
}

/// [`resume`] for the board's `r` and `spoolway queue resume`, which may also
/// restart a row holding a person-answered question: it stands on its own
/// live step with nothing marking it stopped, so the "not stopped" refusal
/// that `spoolway resume` makes would turn that restart away. The board only
/// offers `r` on a row that is resumable, which `spoolway resume` cannot see.
pub(crate) fn resume_held_row(repo: &Repo, pipelines: &Pipelines, args: &ResumeArgs) -> Result<()> {
    resume_checked(repo, pipelines, args, None, true)
}

fn resume_checked(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &ResumeArgs,
    from_step: Option<&str>,
    restart_held_row: bool,
) -> Result<()> {
    let from_blocked = from_step == Some(crate::pipeline::BLOCKED);
    if !from_blocked {
        refuse_from_lane("a gate is answered", from_step.is_some())?;
    } else if args.stage.is_some() {
        bail!(
            "a lane on `blocked` may resume another stopped task, but never with `--stage` — \
             that is a person's to decide."
        );
    }
    let mut task = repo.task(&args.task)?;
    // The lane on `blocked` is working on its own task's block; clearing it
    // would answer its own question for it. It may unblock others only.
    if from_blocked && crate::platform::env_var(TASK_ENV).is_ok_and(|own| own == args.task) {
        bail!(
            "task `{}` is this lane's own — a lane on `blocked` may resume other stopped \
             tasks, never its own.",
            args.task
        );
    }
    if !restart_held_row {
        refuse_if_not_stopped(&task, pipelines)?;
    }
    if args.stage.is_some() {
        refuse_while_dependencies_unfinished(repo, &task)?;
    }
    if from_blocked && task.front.paused_at.is_some() {
        bail!(
            "task `{}` is waiting on a gate — that is a person's to answer, not a lane's.",
            args.task
        );
    }

    // A gate is the one road out of `paused` that answers a *question*
    // rather than a stop — `paused_at` is what tells the two apart, not the
    // stage alone: a `p` park and a resumed block both land on `paused` too,
    // and neither has a gate to answer. `--stage` is checked first: a person
    // names one to reroute a genuine gate, or a park, on purpose, and it
    // always takes the ordinary road onto the step it names.
    let road = match &args.stage {
        Some(stage) => {
            let pipeline = pipelines.for_task(&task)?;
            let dependents = crate::dispatch::queue_dependents(repo, &task)?;
            // A step the task walks past is never written as its stage, so
            // naming one by hand is refused rather than accepted and walked
            // past on the next tick.
            if let Some(why) =
                crate::dispatch::walk_past(pipeline.require_step(stage)?, &task, dependents)
            {
                bail!(
                    "`{stage}` does not run for `{}`: {}. Nothing was resumed.",
                    task.front.id,
                    why.refusal()
                );
            }
            ResumeRoad::Step(stage.clone())
        }
        None => resume_road(&task, pipelines)?,
    };
    match road {
        ResumeRoad::Gate {
            gated,
            caught,
            cleared_block,
            destination,
        } => {
            // Only the roads that land on a step need the pipeline: a parked
            // task or one paused by a failed done hook resumes without it,
            // even when its pipeline is no longer defined.
            let pipeline = pipelines.for_task(&task)?;
            let dependents = crate::dispatch::queue_dependents(repo, &task)?;
            let landing =
                crate::dispatch::land_past_hidden(pipeline, &task, destination, dependents);
            let (destination, walked_past) =
                land_resume(repo, pipeline, &mut task, &gated, landing);
            past_the_gate(
                task,
                args,
                &gated,
                caught,
                cleared_block,
                destination,
                walked_past,
            )
        }
        road => back_onto_its_step(repo, pipelines, task, args, road),
    }
}

/// Land a resume on `landing.destination`, spending the `loop:` of each step
/// walked past and of the landing step when the move walked past anything. A
/// spent budget on a step passed sends the resume to its `loop_exit`, which is
/// `blocked`.
///
/// A resume that passed nothing writes its target exactly as it always has:
/// it refunds nothing and checks nothing. One that walked past a hidden step
/// is the move the dispatcher's fall-through used to make a tick later, and
/// that one answered to the landing step's `loop:`, so it still does.
fn land_resume(
    repo: &Repo,
    pipeline: &Pipeline,
    task: &mut Task,
    from: &str,
    mut landing: crate::dispatch::Landing,
) -> (String, Option<String>) {
    if landing.passed.is_empty() {
        return (landing.destination, None);
    }
    spend_walked_past(pipeline, task, &mut landing, from, repo.unattended());
    let note = landing.note();
    let destination =
        apply_loop_budget(pipeline, task, from, landing.destination, repo.unattended());
    if destination == crate::pipeline::BLOCKED {
        set_blocked_from(task, from);
    }
    (destination, note)
}

/// Refuse a task that is not stopped: one waiting in the queue, or one whose
/// lane is running a pipeline step.
///
/// Resuming a running task rewinds it, and its lane's own later report is then
/// refused. Resuming a queued task with `--stage` would skip the steps in front
/// of the one named. A stage no pipeline defines is left resumable: a pipeline
/// edited while a task sat on a removed step must still be rescued by hand.
fn refuse_if_not_stopped(task: &Task, pipelines: &Pipelines) -> Result<()> {
    let stage = task.stage();
    let running = stage != crate::pipeline::BLOCKED
        && stage != crate::pipeline::PAUSED
        && pipelines
            .for_task(task)
            .is_ok_and(|pipeline| pipeline.step(stage).is_some());
    if stage == crate::pipeline::QUEUED {
        bail!(
            "task `{}` is {stage}, not stopped — it starts on its own when the dispatcher \
             reaches it, so there is nothing to resume.",
            task.front.id
        );
    }
    if running {
        bail!(
            "task `{id}` is {stage}, not stopped — there is nothing to resume. To stop it \
             where it is, run `spoolway queue pause {id}`.",
            id = task.front.id
        );
    }
    Ok(())
}

/// Refuse `resume --stage` while a task in `depends_on` has not finished, that
/// is reached `done` and been archived.
///
/// The dispatcher gates a dependency only on `queued`, so a child sent to a
/// later step by hand would otherwise run and finish ahead of its parent. The
/// check is [`crate::graph::Graph::waiting_on`], the one the dispatcher uses:
/// a dependency that left the queue counts as done only if it is in the
/// archive, so one unqueued back to pending, or misspelled, is refused too.
fn refuse_while_dependencies_unfinished(repo: &Repo, task: &Task) -> Result<()> {
    let tasks = repo.tasks()?;
    let graph = crate::graph::Graph::build(&tasks, &repo.archive_dir());
    let Some(dep) = graph.waiting_on(&task.front.id).into_iter().next() else {
        return Ok(());
    };
    let id = &task.front.id;
    let Some(parent) = tasks.iter().find(|t| t.id() == dep) else {
        bail!(
            "{id} depends on {dep}, which is not a task in the queue or the archive — \
             correct or remove {dep} in {id}'s `depends_on`"
        );
    };
    let state = parent.stage();
    // Only a stopped parent can be resumed; telling a person to resume one
    // that is running or queued would lead to a second refusal.
    let advice = match state == crate::pipeline::BLOCKED || state == crate::pipeline::PAUSED {
        true => format!("resume {dep} first"),
        false => format!("wait for {dep} to finish"),
    };
    // A dependency on `done` is not archived until cleanup and the `done`
    // hook have run; saying only "which is done" would read as a contradiction.
    let state = match state == crate::pipeline::DONE {
        true => "done but not yet cleaned up and archived".to_string(),
        false => state.to_string(),
    };
    bail!("{id} depends on {dep}, which is {state} — {advice}");
}

/// Every road out of a stop but a gate's, once [`resume`] has chosen it.
fn back_onto_its_step(
    repo: &Repo,
    pipelines: &Pipelines,
    mut task: Task,
    args: &ResumeArgs,
    road: ResumeRoad,
) -> Result<()> {
    // A stop's mark is spent by any resume, whoever sends it: left set on a
    // task a person resumed by hand, the next start would find it again —
    // see `crate::status::resume_stop_parked`. Every road below saves.
    task.front.parked_by_stop = false;
    // Whatever stopped it is over once it is sent back. Every road below
    // saves, so the mark lands with the move. A park taken on a blocked task
    // goes back onto `blocked` with its stop still standing, so that road
    // leaves the newest entry unmarked.
    if road.destination() != crate::pipeline::BLOCKED {
        task.mark_blocker_cleared();
    }

    // A `p` park runs none of `resume_at`'s bookkeeping — see `unpark`.
    // Ahead of the hook pause below, which a park leaves standing.
    if let ResumeRoad::Unpark(step) = &road {
        return unpark(repo, pipelines, task, step.clone());
    }

    // A pause over a start branch that did not exist is answered by sending
    // the task back to `queued`, which `resume_target` already does for a task
    // that never started. The marker only says why it paused, so it goes
    // either way: left set, a later pause would read as this one.
    task.front.missing_start_branch = None;

    // A hook pause is neither a block nor a park: nothing inside the
    // pipeline failed a check, a hook exited non-zero on `queued`, `started`
    // or `done`, or was killed three times in a row without an exit code —
    // see `crate::dispatch::Dispatcher::pause_for_hook_failure`. Forgetting
    // the failed run is always right, whichever road the rest of this
    // function takes, so it happens ahead of everything else — `resume` is
    // the one place a hook pause is ever undone. For the kill pause this
    // forget is also what resets the kill count, so the hook is fired afresh.
    if let Some(stage) = task.front.hook_paused.take() {
        crate::tracking::forget(repo, &task, &stage);
    }
    if road == ResumeRoad::HookDone {
        task.set_stage(
            crate::pipeline::DONE,
            Some("hook run forgotten by `spoolway resume`"),
        );
        task.save()?;
        free_stale_lanes(repo, pipelines, &task);
        println!("{}: -> {}", args.task, crate::pipeline::DONE);
        return Ok(());
    }

    // Lands past the steps this task does not run. `queued` is not a step,
    // so a task that never started lands as it is.
    let pipeline = pipelines.for_task(&task)?;
    let dependents = crate::dispatch::queue_dependents(repo, &task)?;
    let landing = crate::dispatch::land_past_hidden(
        pipeline,
        &task,
        road.destination().to_string(),
        dependents,
    );
    let target = landing.destination.clone();

    // A park off `queued` itself lands here too — `park` records no
    // `parked_from` for it — and it is the same kind of round trip as
    // `unpark`'s: the task never left `queued`, so sending it back is not a
    // lap of anything the pipeline routed. `queued` is also not a step any
    // pipeline declares, so `resume_at` and `set_stage` below — built for a
    // real step's `on_pass`/loop bookkeeping — are the wrong road for it
    // regardless.
    if road == ResumeRoad::Queued {
        task.front.paused_at = None;
        task.front.paused_by = None;
        task.set_stage_unbanked(crate::pipeline::QUEUED, "put back from the board");
        task.save()?;
        free_stale_lanes(repo, pipelines, &task);
        println!("{}: -> {target}", args.task);
        return Ok(());
    }

    let message = args
        .message
        .clone()
        .unwrap_or_else(|| "unblocked by hand".to_string());
    // A person's resume of a task held on `blocked` is a road out of it like
    // the unblocker's pass: the counts that stopped it start again, so the
    // step it goes back to is not walked straight into a spent limit.
    if task.front.stage == crate::pipeline::BLOCKED {
        task.reset_loop_counts();
    }
    // After that reset, so a walk-past from `blocked` meets the landing
    // step's `loop:` with its counts started again.
    let current = task.stage().to_string();
    let (target, walked_past) = land_resume(repo, pipeline, &mut task, &current, landing);
    resume_at(&mut task, &target);
    // Whatever gate it was waiting on, it is not waiting on it here any more.
    task.front.paused_at = None;
    task.front.paused_by = None;
    // A `--stage` reroute past a park leaves this ordinary road instead of
    // `unpark`'s, but the park is answered all the same — left set, this
    // would still name the step on a later, ordinary retry, and
    // `finish_launch_bookkeeping` would read that retry as a continued park rather than what it is.
    task.front.parked_from = None;
    task.front.escalated = false;
    let arrival = crate::dispatch::with_walk_note(Some(&message), walked_past);
    task.set_stage(&target, arrival.as_deref());
    task.save()?;

    free_stale_lanes(repo, pipelines, &task);

    println!("{}: -> {target}", args.task);
    Ok(())
}

/// Put a `parked_from` task back on the step it never left — the road
/// `back_onto_its_step` takes instead of `resume_at` when nothing actually
/// failed a check: a person's own keypress or Escape, or a lane
/// `escalate_clock` gave up on. One code path either way in: the `(next)`
/// row of the board's `r` picker calls this same `resume` with no `--stage`
/// of its own, the same as a bare `spoolway resume <task>` does, and
/// [`resume_road`] is what finds `parked_from` and sends it here.
///
/// `parked_from` (and `escalated` beside it) are left in the task file rather
/// than cleared here — the launch that actually continues this step is what
/// learns whether a session was there to carry, and `finish_launch_bookkeeping`
/// in `src/dispatch.rs` is what spends both once that answer is known, the
/// same moment it spends `resume`.
fn unpark(repo: &Repo, pipelines: &Pipelines, mut task: Task, step: String) -> Result<()> {
    // A continuing lane picks up its own session rather than opening a fresh
    // one — the same one-shot flag a real resume sets, so the launch cannot
    // tell a park from a block apart any other way.
    task.front.resume = Some(step.clone());
    task.set_stage_unbanked(&step, "put back from the board");
    task.save()?;

    free_stale_lanes(repo, pipelines, &task);

    println!("{}: -> {step}", task.front.id);
    Ok(())
}

/// The settled lane on the step a resume sends this task to, closed, so the
/// dispatcher does not mistake one left over from before a stop for a lane
/// still waiting on an answer — the task would wait on it forever and no fresh
/// lane would ever start. Shared by `back_onto_its_step` and `unpark`, the two
/// roads that put a task back on a step a lane of its own might already be
/// sitting settled on.
///
/// Only that one lane. The task's other lanes belong to steps it has already
/// run, and each stays open, idle, until the task is done; closing them here
/// would throw away the very screen a person resumed the task to read. A
/// resume that sends the task somewhere with no lane yet closes nothing.
///
/// Only a lane working in this project's directories — the same ownership
/// rules a pass applies. A busy one is left alone: work that is genuinely
/// running is the dispatcher's to watch, not ours to kill.
fn free_stale_lanes(repo: &Repo, pipelines: &Pipelines, task: &Task) {
    let Ok(mux) = crate::mux::backend(repo) else {
        return;
    };
    if let Ok(lanes) = mux.list_lanes() {
        let step_ids = pipelines.all_step_ids();
        for lane in task_lanes(repo, pipelines, task, lanes) {
            let on_the_resumed_step = crate::mux::parse_lane_name(&lane.name, &step_ids)
                .is_some_and(|(step, _)| step == task.stage());
            if on_the_resumed_step && lane.status.is_settled() {
                let _ = mux.stop_lane(&lane.name, &lane.pane_id);
                println!("  freed stale lane `{}`", lane.name);
            }
        }
    }
}

/// The lanes in `lanes` that belong to `task`: named for it, and working in
/// this project's directories. The one ownership rule `free_stale_lanes` and
/// `spoolway restart` share, so a lane one of them would end is a lane the
/// other would too.
fn task_lanes(
    repo: &Repo,
    pipelines: &Pipelines,
    task: &Task,
    lanes: Vec<crate::mux::Lane>,
) -> Vec<crate::mux::Lane> {
    let step_ids = pipelines.all_step_ids();
    lanes
        .into_iter()
        .filter(|lane| {
            let ours =
                lane.cwd == repo.root || Some(&lane.cwd) == task.front.worktree_path.as_ref();
            let this_task = crate::mux::parse_lane_name(&lane.name, &step_ids)
                .is_some_and(|(_, task_id)| task_id == task.front.id);
            ours && this_task
        })
        .collect()
}

/// The step `spoolway restart` starts over for `task`, or the reason there is
/// none.
///
/// A running task is on its step. A task on `blocked` or `paused` is held
/// *off* the step it stopped on, and the step is read the way a resume reads
/// it: a park's `parked_from`, a gate's `paused_at`, otherwise
/// [`resume_target`]. A task that never started has no step to start over,
/// and neither has one held by a hook, whose pause is on `queued` or `done`.
///
/// `pub(crate)` for the board's `s`, which opens its panel only where this
/// finds a step, so the panel never offers a restart this would refuse.
pub(crate) fn restart_step(task: &Task, pipeline: &Pipeline) -> Result<String> {
    let id = &task.front.id;
    let stage = task.stage();
    if stage == crate::pipeline::QUEUED {
        bail!(
            "task `{id}` is {stage}, so no step has started — it starts on its own when the \
             dispatcher reaches it. To hold it back, run `spoolway queue pause {id}`."
        );
    }
    if task.front.hook_paused.is_some() {
        bail!(
            "task `{id}` is held by a hook, not on a step — run `spoolway resume {id}` to \
             forget the hook run."
        );
    }
    let step = match stage {
        crate::pipeline::PAUSED => task
            .front
            .parked_from
            .clone()
            .or_else(|| task.front.paused_at.clone())
            .unwrap_or_else(|| resume_target(task, pipeline)),
        crate::pipeline::BLOCKED => resume_target(task, pipeline),
        running => running.to_string(),
    };
    if step == crate::pipeline::QUEUED {
        bail!(
            "task `{id}` is {stage} but never started a step, so there is no conversation to \
             start over — run `spoolway resume {id}` to put it back."
        );
    }
    // `done` is a reserved stage no pipeline declares, so it must be told apart
    // from a step that was removed from the pipeline before the lookup below
    // calls a finished task undefined.
    if step == crate::pipeline::DONE {
        bail!(
            "task `{id}` is {step}, which has finished — there is no step left to start \
             over. To run a step again, use `spoolway resume {id} --stage <step>`."
        );
    }
    let Some(declared) = pipeline.step(&step) else {
        bail!(
            "task `{id}` is on `{step}`, which pipeline `{}` does not define — run \
             `spoolway resume {id} --stage <step>` to send it to one that exists.",
            pipeline.name
        );
    };
    match declared.kind() {
        crate::pipeline::StepKind::Command => bail!(
            "task `{id}` is on `{step}`, which runs a command and holds no conversation — \
             run `spoolway resume {id}` to run it again."
        ),
        crate::pipeline::StepKind::Agent => Ok(step),
    }
}

/// Start the step a task is on over with a fresh conversation: the road onto a
/// step that ends the step's session rather than continuing it.
///
/// Every other road back onto a step continues the conversation the step
/// already has, so a conversation that is itself the problem is walked back
/// into by a resume and by the next retry alike. This writes `restart:` for
/// the dispatcher's next launch of the step to read, and sends the task back
/// to the step it was already on.
///
/// Whatever lane the task owns is ended first, [`restart_with`] explains how.
/// The worktree is left exactly as it is.
pub fn restart(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &RestartArgs,
    from_step: Option<&str>,
) -> Result<()> {
    refuse_from_lane("a step is restarted", from_step.is_some())?;
    let mux = crate::mux::backend(repo)?;
    restart_with(repo, pipelines, args, mux.as_ref())
}

/// [`restart`] over the backend it is handed.
///
/// The task file is written before any lane is touched. A dispatcher pass
/// lists lanes and reads tasks, and one that landed between a teardown and a
/// later write would see the task on its step with no lane and relaunch it
/// from a snapshot that has no `restart:`, carrying the very conversation
/// being abandoned. Written first, a pass that still sees the old lane working
/// leaves it alone, and one that sees it gone reads `restart:` and opens
/// fresh. The same order is `status::interrupt_for_stop`'s: park, then abort.
///
/// The write goes through [`crate::dispatch::persist_task`], which refuses it
/// when the task file changed since it was read. A lane's `spoolway report`
/// landing in that gap is the newer fact, and overwriting it would put the
/// task back on a step the report has already moved it off. So this refuses and
/// says so, with every lane still as it was, instead of printing a restart that
/// did not happen.
///
/// Then the lane on the restarted step is ended, and the lane on the step the
/// task stands on when that is another one, such as a blocked task's
/// unblocker: one that is mid-turn or waiting on a prompt is interrupted
/// first, and both are stopped. The task's lanes on steps it has already run
/// are kept, as a resume keeps them, until the task is done. Stopping is not
/// conditioned on `resident_while_waiting()`, the way an escalation's teardown
/// is: under a backend that keeps a stopped pane, the pane holds the very
/// conversation being discarded, and keeping it would leave a second session
/// running on the same task.
pub(crate) fn restart_with(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &RestartArgs,
    mux: &dyn crate::mux::Mux,
) -> Result<()> {
    let mut task = repo.task(&args.task)?;
    let pipeline = pipelines.for_task(&task)?;
    let step = restart_step(&task, pipeline)?;
    let mut seen = std::collections::HashMap::from([(
        task.front.id.clone(),
        crate::dispatch::file_fingerprint(&task),
    )]);

    // Read before the lane goes: the dispatcher's own record of it is dropped
    // once the lane is gone, and the ledger is what outlives that.
    let session = abandoned_session(repo, &step, &task.front.id);
    let step_ids = pipelines.all_step_ids();
    let lanes: Vec<_> = task_lanes(repo, pipelines, &task, mux.list_lanes()?)
        .into_iter()
        .filter(|lane| {
            crate::mux::parse_lane_name(&lane.name, &step_ids)
                .is_some_and(|(on, _)| on == step || on == task.stage())
        })
        .collect();

    // A stop's marks are spent by a restart as by a resume: left set, they
    // would describe a stop the task is no longer in.
    task.front.parked_by_stop = false;
    task.mark_blocker_cleared();
    task.front.missing_start_branch = None;
    task.front.paused_at = None;
    task.front.paused_by = None;
    task.front.parked_from = None;
    task.front.blocked_from = None;
    task.front.escalated = false;
    // `restart` supersedes `resume`: both set would name a continuation and an
    // abandonment for the same launch.
    task.front.resume = None;
    // Leaving `blocked` starts the loop counts again, as a resume does, so the
    // step is not walked straight into the limit that stopped it.
    if task.front.stage == crate::pipeline::BLOCKED {
        task.reset_loop_counts();
    }
    task.front.restart = Some(step.clone());
    let message = args
        .message
        .clone()
        .unwrap_or_else(|| "restarted by hand — fresh session".to_string());
    task.set_stage_unbanked(&step, &message);
    if !crate::dispatch::persist_task(repo, &mut task, &mut seen)? {
        bail!(
            "task `{}` changed while the restart was being written — a report landed first, so \
             the restart was not written and no lane was touched. Check `spoolway queue show {}` \
             and run it again if it still applies.",
            args.task,
            args.task
        );
    }

    for lane in lanes {
        // Spelled out rather than `is_busy()`, as in the dispatcher's own
        // teardown: a lane parked on a permission prompt is still spending.
        if matches!(
            lane.status,
            crate::mux::LaneStatus::Working | crate::mux::LaneStatus::Blocked
        ) {
            let _ = mux.interrupt_lane(&lane.name);
        }
        mux.stop_lane(&lane.name, &lane.pane_id).with_context(|| {
            format!(
                "the restart is written, but lane `{}` could not be stopped — run \
                 `spoolway restart {}` again to stop it",
                lane.name, args.task
            )
        })?;
        println!("  tore down lane `{}`", lane.name);
    }
    if let Some(session) = &session {
        println!("  session {}", session.describe());
    }
    println!("{}: -> {step} (fresh session)", args.task);
    Ok(())
}

/// The conversation a restart of `step` on task `id` throws away, if one is
/// on record.
///
/// Shared by `spoolway restart`'s own report and the board's `s` panel, so
/// the session a person confirms is the one the command says it abandoned.
pub(crate) fn abandoned_session(repo: &Repo, step: &str, id: &str) -> Option<AbandonedSession> {
    let ledger = crate::usage::read(repo).unwrap_or_default();
    let lane = crate::mux::lane_name(step, id);
    let (_, session) = crate::dispatch::lane_session_in(repo, &ledger, &lane)?;
    let banked = ledger.iter().any(|entry| entry.session == session);
    Some(AbandonedSession {
        id: session,
        banked,
    })
}

/// See [`abandoned_session`].
pub(crate) struct AbandonedSession {
    /// The session id in full.
    pub(crate) id: String,
    /// Whether the usage ledger already holds this session, so what it spent
    /// is counted even though the conversation is gone.
    pub(crate) banked: bool,
}

impl AbandonedSession {
    /// `32b0d7bd — abandoned, already banked`: the id cut to its first
    /// eight characters, and `already banked` only when the ledger holds it.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} — abandoned{}",
            self.id.chars().take(8).collect::<String>(),
            if self.banked { ", already banked" } else { "" }
        )
    }
}

/// The person's half of a gate: let a paused task past its gated step, or send
/// it back round.
///
/// This is the answer `crate::pipeline::Step::gate` waits for. The task's work
/// is done and committed — a gated lane reports like any other, and its report
/// went through [`report`] in full — so nothing here runs, rebuilds or checks
/// anything. All that is left is the routing decision the pass was not allowed
/// to take on its own, which [`resume_road`] has already made.
fn past_the_gate(
    mut task: Task,
    args: &ResumeArgs,
    gated: &str,
    caught: Option<Caught>,
    cleared_block: bool,
    destination: String,
    walked_past: Option<String>,
) -> Result<()> {
    if cleared_block {
        resume_at(&mut task, &destination);
    }
    // A gate can sit over a caught block, whose entry is over once a person
    // answers. Answered onto `blocked` itself, the stop is still standing.
    if destination != crate::pipeline::BLOCKED {
        task.mark_blocker_cleared();
    }

    // `blocked_from` naming `gated` stops describing where this task is
    // stopped the moment it moves anywhere but `blocked` itself — left
    // standing, it would outlive this answer and read as a caught block the
    // next time this same step is gated and passes cleanly (review finding
    // 4). Only a destination of `blocked` itself still needs it; `resume_at`,
    // for `cleared_block`, already clears it on its own road.
    if destination != crate::pipeline::BLOCKED && task.front.blocked_from.as_deref() == Some(gated)
    {
        task.front.blocked_from = None;
    }

    let note = args.message.clone().unwrap_or_else(|| match cleared_block {
        true => format!("block cleared by hand; nothing was done at `{gated}`"),
        false => format!("`{gated}` released at the gate"),
    });

    task.front.paused_at = None;
    task.front.paused_by = None;
    let arrival = crate::dispatch::with_walk_note(Some(&note), walked_past);
    task.set_stage(&destination, arrival.as_deref());
    task.save()?;

    // A cleared block usually lands on `gated` itself, and the plain
    // `{gated} --resume--> {destination}` phrasing below would then print a
    // step arrowing to itself, reading like a pass that ran and landed
    // nowhere rather than a block being cleared. When the task walks past
    // `gated`, `destination` is a later step; the Status Log names the walk,
    // and this line still says only that the block was cleared.
    if cleared_block {
        println!("{}: {gated}: block cleared by hand", args.task);
    } else {
        // `resume` rather than `outcome`'s own `pass`: this names the
        // person's answer, while the label retains the outcome the schedule
        // caught. A plain gated pass still reads as it always has.
        let label = match caught {
            Some(Caught::Blocked) => format!("{gated} {}", crate::pipeline::BLOCKED),
            Some(Caught::Fail) => format!("{gated} failed"),
            _ => gated.to_string(),
        };
        println!("{}: {label} --resume--> {destination}", args.task);
    }
    Ok(())
}

/// A task id from the argument, or from the environment every lane is given.
///
/// `report` is meant to run from inside a lane, so it cannot refuse a lane
/// environment wholesale the way `resume` and `queue add` do. But a `--task`
/// that names a *different* task than `SPOOLWAY_TASK` is a lane reporting on a
/// sibling's step — crediting it with a pass that never ran there (review
/// finding 10) — and is refused here, the same class of refusal as
/// [`crate::commands::refuse_from_lane`].
///
/// Whatever the id's source, it is run through [`crate::config::check_id`]
/// before it is handed on: `--task ../../other/queue/x` would otherwise be
/// joined into a path and the file outside the queue read and rewritten.
fn resolve_task_id(explicit: Option<&str>) -> Result<String> {
    let from_env = crate::platform::env_var(TASK_ENV)
        .ok()
        .filter(|v| !v.is_empty());
    let id = match (explicit, from_env.as_deref()) {
        (Some(explicit), Some(env)) if explicit != env => bail!(
            "this lane was started for `{env}`, but `--task {explicit}` names another task — \
             a lane may only report on its own task. Ask the person watching the board."
        ),
        (Some(explicit), _) => explicit.to_string(),
        (None, Some(env)) => env.to_string(),
        (None, None) => {
            bail!("no task given and ${TASK_ENV} is not set — pass the task id explicitly")
        }
    };
    crate::config::check_id("task id", &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::*;

    /// Every choice a stop offers is printed key first, then the command
    /// that does the same thing, and no offered choice lacks a key —
    /// acceptance criterion 3, checked against the exact three lines a
    /// gated pass or a caught block prints into the pane it just parked.
    #[test]
    fn stop_choices_prints_every_action_key_first() {
        let choices = stop_choices("confirm-dialog", "look");
        assert_eq!(
            choices,
            "\n\n  resume         [r]   spoolway resume confirm-dialog\n  \
             open the pane  [o]   spoolway lane 'confirm-dialog · look' --attach\n  \
             park it        [p]   spoolway queue pause confirm-dialog"
        );
        for line in choices.lines().filter(|l| l.contains("spoolway")) {
            assert!(
                line.trim_start().starts_with(char::is_alphabetic) && line.contains('['),
                "{line}"
            );
        }
    }

    /// The rule that decides whether leftovers in a worktree are work to rescue
    /// or residue to leave alone.
    ///
    /// Found end to end: a lane that had committed its work properly also left a
    /// `Cargo.lock` a test run generated, the backstop swept it into a commit,
    /// and the reviewer failed the change for touching a file the task's
    /// non-goals forbade — round after round, with nothing downstream able to
    /// break the loop.
    #[test]
    fn only_a_lane_that_committed_nothing_gets_its_worktree_committed_for_it() {
        // Committed something: whatever is left is residue.
        assert!(lane_committed("aaa111", "bbb222"));
        // Committed nothing all turn: this is the whole of its work.
        assert!(!lane_committed("aaa111", "aaa111"));
        // Nobody recorded where the branch started — commit, rather than risk
        // discarding a turn's work on a guess.
        assert!(!lane_committed("", "aaa111"));
    }

    /// The one behaviour spoolway takes on around git, end to end: an
    /// uncommitted worktree, and a commit whose subject says which task and
    /// which step left it there.
    ///
    /// The message shape is part of the contract, not decoration. A stacked pull
    /// request is read by a person, and `wip(<task>): <step>` is what tells them
    /// which commits the pipeline wrote from the ones a lane meant.
    #[test]
    fn a_settling_step_has_its_leftovers_committed_as_wip() {
        let (repo, _root_guard) = fixture("auto-commit");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(repo.root.join("seed"), "seed").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "seed"]);
        let head = crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"]).unwrap();
        let head = head.trim().to_string();

        std::fs::write(repo.root.join("work"), "what the lane did").unwrap();
        let outcome = auto_commit(&repo, &repo.root, &head, "task-1", "implement");
        assert!(matches!(outcome, AutoCommit::Committed(_)), "{outcome:?}");
        assert!(
            outcome.note().unwrap().contains("committed 1 file"),
            "{outcome:?}"
        );

        let subject = crate::repo::run(&repo.root, "git", &["log", "-1", "--format=%s"]).unwrap();
        assert_eq!(subject.trim(), "wip(task-1): implement");

        // Nothing left to pick up, so nothing to report.
        let head = crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"]).unwrap();
        assert!(matches!(
            auto_commit(&repo, &repo.root, head.trim(), "task-1", "review"),
            AutoCommit::Clean
        ));
    }

    /// A lane may only report on its own task. `cargo test` runs inside a real
    /// lane's own environment, so `SPOOLWAY_TASK` is already set to *that*
    /// lane's task when `report()` runs from a test using a fixture id —
    /// standing in here for a lane at `implement` for `stuck` that calls
    /// `spoolway report --task task-1 --pass` to credit a sibling with a step
    /// that never ran on it (review finding 10). The report is refused
    /// outright, and nothing about `task-1` moves.
    #[test]
    fn a_report_is_refused_when_task_differs_from_spoolway_task() {
        let (repo, _root_guard) = fixture("report-wrong-task");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "task-1", &[]);
        let mut task = queued(&repo, "task-1");
        task.set_stage("implement", None);
        task.save().unwrap();

        // Thread-local, not `set_test_env`: `SPOOLWAY_TASK` is read on every
        // `report()` call, in a module whose own tests call `report()` from
        // dozens of other threads at once — a process-global set here would
        // hand a neighbour's call a task id it never asked for. See
        // `crate::platform::env_var`.
        let err = crate::platform::test_env::with_env(TASK_ENV, "stuck", || {
            report(
                &repo,
                &Pipelines::builtin(),
                &ReportArgs {
                    task: Some("task-1".into()),
                    stage: None,
                    pass: true,
                    fail: false,
                    block: false,
                    pause: false,
                    message: None,
                    handoff: vec![],
                },
                None,
            )
        })
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("stuck") && err.contains("task-1"), "{err}");

        // And `task-1` is exactly where it was: still at `implement`, no
        // report recorded.
        let task = queued(&repo, "task-1");
        assert_eq!(task.stage(), "implement");
        assert!(task.front.last_report.is_none());
    }

    /// Off, the residue is still named — the point of the setting is to stop
    /// spoolway writing history, not to stop it telling you what is there.
    #[test]
    fn auto_commit_off_reports_the_leftovers_and_commits_nothing() {
        let (mut repo, _root_guard) = fixture("auto-commit-off");
        repo.config.dispatch.auto_commit = false;
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(repo.root.join("seed"), "seed").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "seed"]);
        let head = crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"]).unwrap();

        // A tracked file changed is work: held, not swept.
        std::fs::write(repo.root.join("seed"), "what the lane did").unwrap();
        let outcome = auto_commit(&repo, &repo.root, head.trim(), "task-1", "implement");
        assert!(outcome.is_unrecorded(), "{outcome:?}");
        let note = outcome
            .note()
            .expect("the leftovers are still worth naming");
        assert!(note.contains("auto_commit` is off"), "{note}");

        // Untracked leftovers alone are residue — lifecycle review finding
        // 4: a lockfile or a build artefact must not hold a finished task at
        // `blocked` with the backstop off, any more than it does with it on.
        std::fs::write(repo.root.join("seed"), "seed").unwrap();
        std::fs::write(repo.root.join("work.lock"), "generated").unwrap();
        let outcome = auto_commit(&repo, &repo.root, head.trim(), "task-1", "implement");
        assert!(!outcome.is_unrecorded(), "{outcome:?}");
        assert!(
            outcome
                .note()
                .is_some_and(|n| n.contains("untracked") && n.contains("auto_commit` is off")),
            "{outcome:?}"
        );
        assert_eq!(
            crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"])
                .unwrap()
                .trim(),
            head.trim(),
            "nothing should have been committed"
        );
    }

    /// The verdict a lane reports has to survive until the dispatcher banks
    /// that lane's usage, and the two run in different processes — so the task
    /// file is the only channel between them.
    ///
    /// Stored with the step it belongs to, because a lane that dies without
    /// reporting must be banked as having *no* outcome rather than inheriting
    /// the previous step's. A silent death recorded as a pass is worse than no
    /// record at all: it is the one shape of bad news the ledger exists to
    /// show.
    #[test]
    fn a_report_leaves_its_verdict_for_the_ledger_tagged_with_its_step() {
        let (repo, _root_guard) = fixture("report-leaves-verdict");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "login", &[]);

        let mut task = queued(&repo, "login");
        task.set_stage("review", None);
        task.save().unwrap();

        report(
            &repo,
            &Pipelines::builtin(),
            &ReportArgs {
                task: Some("login".into()),
                stage: None,
                pass: false,
                fail: true,
                block: false,
                pause: false,
                message: Some("the tests do not cover the new branch".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap();

        let left = queued(&repo, "login").front.last_report.unwrap();
        assert_eq!(left.outcome, "fail");
        assert_eq!(
            left.step, "review",
            "the step is what lets a later silent lane be told apart from this one"
        );
        assert!(
            left.at > 0 && (chrono::Utc::now().timestamp() - left.at).abs() < 5,
            "at has to be stamped from the same clock the dispatcher reads a \
             lane's own started_at from: {}",
            left.at
        );
    }

    /// `--handoff` is independent of the outcome: it lands under `##
    /// Handoff`, credited to the step that reported it, in the same save
    /// that routes the task — repeatable, one line per flag.
    #[test]
    fn a_report_appends_each_handoff_line_credited_to_its_step() {
        let (repo, _root_guard) = fixture("report-carries-handoff");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "login", &[]);

        let mut task = queued(&repo, "login");
        task.set_stage("implement", None);
        task.save().unwrap();

        report(
            &repo,
            &Pipelines::builtin(),
            &ReportArgs {
                task: Some("login".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("done".into()),
                handoff: vec![
                    "the migration script wants a dry run first".into(),
                    "the ops runbook still names the old queue".into(),
                ],
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "login");
        let handoff = task.section("## Handoff").unwrap();
        assert!(
            handoff.contains("- `implement` — the migration script wants a dry run first"),
            "{handoff}"
        );
        assert!(
            handoff.contains("- `implement` — the ops runbook still names the old queue"),
            "{handoff}"
        );
    }

    /// A lane's `--block` must leave the same trail the dispatcher's own
    /// escalation leaves: the step it stopped on. `spoolway resume` resumes
    /// from that, and without it the task restarts at the pipeline's entry —
    /// which for a task that blocked at `pr` means a second branch push and a
    /// second pull request.
    #[test]
    fn a_reported_block_records_the_step_it_stopped_on() {
        // Both roads out of a block read this: `spoolway resume` by hand, and
        // an unattended run's own resume. Read here in an attended run, where
        // the task really does come to rest on `blocked` and the trail is the
        // only thing that will ever say where it stopped.
        let (repo, _root_guard) = fixture("block-records-step");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("handover", None);
        task.save().unwrap();

        report(
            &repo,
            &Pipelines::builtin(),
            &ReportArgs {
                task: Some("stuck".into()),
                stage: None,
                pass: false,
                fail: false,
                block: true,
                pause: false,
                message: Some("the forge is down".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "blocked");
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("handover"),
            "resume must be able to continue at `pr` rather than re-running the whole flow"
        );

        // And resuming really does resume there.
        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(queued(&repo, "stuck").stage(), "handover");
    }

    /// Writes an executable `.spoolway/hooks/<name>` — the fixture the two
    /// hook-pause resume tests below share. Mirrors `dispatch::tests::
    /// write_hook`, kept as its own copy rather than shared across modules
    /// for one private test helper.
    fn write_hook(repo: &Repo, name: &str, script: &str) {
        let dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    /// Polls `crate::tracking::exit_code` until the hook run for `event`
    /// settles — the hook is spawned detached, so nothing here waits on it
    /// directly.
    fn wait_for_exit_code(repo: &Repo, task: &Task, event: &str) -> i32 {
        for _ in 0..200 {
            if let Some(code) = crate::tracking::exit_code(repo, task, event) {
                return code;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("hook run for `{event}` never settled");
    }

    /// The text of every `spoolway doctor` row naming a failed task hook —
    /// the rows the "before dispatching" popup shows under `problems`.
    fn hook_rows(repo: &Repo) -> Vec<String> {
        crate::commands::doctor::issue_tracking_checks(repo, &repo.config.issue_tracking)
            .into_iter()
            .filter_map(|finding| match finding {
                crate::commands::doctor::Finding::Check(label, Err(err))
                    if label == crate::commands::doctor::HOOK_FAILURE_LABEL =>
                {
                    Some(format!("{err:#}"))
                }
                _ => None,
            })
            .collect()
    }

    /// Asserts the one hook row there is names `spoolway resume demo` as its
    /// remedy — what the three tests below then run, and prove clears it.
    fn assert_row_says_resume(repo: &Repo, event: &str) {
        let rows = hook_rows(repo);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].starts_with(&format!("`{event}` failed for demo (exit 1)")),
            "{rows:?}"
        );
        assert!(rows[0].contains("`spoolway resume demo`"), "{rows:?}");
    }

    /// Acceptance criterion: `spoolway resume` on a task a failing `queued`
    /// hook paused forgets that run, so the very next `fire` starts it over
    /// rather than reading the same stale exit code and pausing the task
    /// right back.
    #[test]
    fn resume_forgets_a_failed_queued_hooks_run() {
        let (mut repo, _root_guard) = fixture("hook-resume-queued");
        write_hook(&repo, "fail.sh", "exit 1");
        add(&repo, "demo", &[]);
        repo.config.issue_tracking.hook = "fail.sh".into();

        let mut task = queued(&repo, "demo");
        crate::tracking::fire(&repo, &task, crate::pipeline::QUEUED, 1).unwrap();
        assert_eq!(wait_for_exit_code(&repo, &task, crate::pipeline::QUEUED), 1);

        // What `Dispatcher::pause_for_hook_failure` would have written.
        task.front.hook_paused = Some(crate::pipeline::QUEUED.to_string());
        task.set_stage(
            crate::pipeline::PAUSED,
            Some("issue_tracking hook exited 1 on `queued`"),
        );
        task.save().unwrap();

        assert_row_says_resume(&repo, crate::pipeline::QUEUED);

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), crate::pipeline::QUEUED);
        assert!(
            task.front.hook_paused.is_none(),
            "the marker must be spent by the resume that reads it"
        );
        assert_eq!(
            crate::tracking::exit_code(&repo, &task, crate::pipeline::QUEUED),
            None,
            "the failed run must be forgotten, or the very next pass pauses it right back"
        );
        assert!(
            hook_rows(&repo).is_empty(),
            "the remedy the row names must clear it: {:?}",
            hook_rows(&repo)
        );
    }

    /// A failing `paused` hook only records its failure, so nothing holds the
    /// task for it and `spoolway resume` forgets nothing: the row says so
    /// rather than naming a resume, and is still there after one.
    #[test]
    fn resume_leaves_a_failed_paused_hooks_row_and_the_row_says_so() {
        let (mut repo, _root_guard) = fixture("hook-resume-paused");
        write_hook(&repo, "fail.sh", "exit 1");
        add(&repo, "demo", &[]);
        repo.config.issue_tracking.hook = "fail.sh".into();

        let mut task = queued(&repo, "demo");
        task.set_stage(crate::pipeline::PAUSED, Some("held by hand"));
        task.save().unwrap();
        crate::tracking::fire(&repo, &task, crate::pipeline::PAUSED, 1).unwrap();
        assert_eq!(wait_for_exit_code(&repo, &task, crate::pipeline::PAUSED), 1);

        let before = hook_rows(&repo);
        assert_eq!(before.len(), 1, "{before:?}");
        assert!(
            before[0].contains("once the task is done and archived"),
            "{before:?}"
        );
        assert!(!before[0].contains("`spoolway resume demo`"), "{before:?}");

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(queued(&repo, "demo").stage(), crate::pipeline::QUEUED);
        assert_eq!(hook_rows(&repo), before, "resume does not clear it");
    }

    /// A task paused over a start branch that does not exist goes back to
    /// `queued` with the marker cleared, and prints the usual line.
    #[test]
    fn resume_puts_a_missing_start_branch_pause_back_on_queued() {
        let (repo, _root_guard) = fixture("resume-missing-start-branch");
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.front.missing_start_branch = Some("task/gone".to_string());
        task.set_stage(crate::pipeline::PAUSED, Some("start branch missing"));
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), crate::pipeline::QUEUED);
        assert!(task.front.missing_start_branch.is_none());
    }

    /// Acceptance criterion: a task paused on `started` — the task never
    /// left `queued`, so it has no worktree, no lane and no `last_report`
    /// — goes back to `queued`, exactly the road a `queued` hold already
    /// takes, and forgets `started`'s own run rather than `queued`'s.
    #[test]
    fn resume_forgets_a_failed_started_hooks_run_and_goes_back_to_queued() {
        let (mut repo, _root_guard) = fixture("hook-resume-started");
        write_hook(&repo, "fail.sh", "exit 1");
        add(&repo, "demo", &[]);
        repo.config.issue_tracking.hook = "fail.sh".into();

        let mut task = queued(&repo, "demo");
        crate::tracking::fire(&repo, &task, crate::pipeline::STARTED, 1).unwrap();
        assert_eq!(
            wait_for_exit_code(&repo, &task, crate::pipeline::STARTED),
            1
        );

        // What `Dispatcher::pause_for_hook_failure` would have written.
        task.front.hook_paused = Some(crate::pipeline::STARTED.to_string());
        task.set_stage(
            crate::pipeline::PAUSED,
            Some("issue_tracking hook exited 1 on `started`"),
        );
        task.save().unwrap();

        assert_row_says_resume(&repo, crate::pipeline::STARTED);

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(
            task.stage(),
            crate::pipeline::QUEUED,
            "a task paused on `started` never left `queued`, so that is where it goes back to"
        );
        assert!(
            task.front.hook_paused.is_none(),
            "the marker must be spent by the resume that reads it"
        );
        assert_eq!(
            crate::tracking::exit_code(&repo, &task, crate::pipeline::STARTED),
            None,
            "the failed run must be forgotten, or the very next pass pauses it right back"
        );
        assert!(
            hook_rows(&repo).is_empty(),
            "the remedy the row names must clear it: {:?}",
            hook_rows(&repo)
        );
    }

    /// Acceptance criterion: a task paused on `done` goes back to `done`,
    /// not to `queued` or a step — `resume_target`'s ordinary roads only
    /// know pipeline steps and `queued`, neither of which `done` is.
    #[test]
    fn resume_on_a_done_hook_pause_forgets_the_run_and_goes_back_to_done() {
        let (mut repo, _root_guard) = fixture("hook-resume-done");
        write_hook(&repo, "fail.sh", "exit 1");
        add(&repo, "demo", &[]);
        repo.config.issue_tracking.hook = "fail.sh".into();

        let mut task = queued(&repo, "demo");
        // A `done` hold happens only once the task has moved all the way
        // through its pipeline — `last_report` would otherwise steer
        // `resume_target` at whatever step reported last, exactly the
        // step-shaped road this task's `hook_paused` marker is for skipping.
        task.front.last_report = Some(crate::task::LastReport {
            step: "work".to_string(),
            outcome: "pass".to_string(),
            at: 0,
            blocked: false,
        });
        task.set_stage(crate::pipeline::DONE, None);
        crate::tracking::fire(&repo, &task, crate::pipeline::DONE, 1).unwrap();
        assert_eq!(wait_for_exit_code(&repo, &task, crate::pipeline::DONE), 1);

        task.front.hook_paused = Some(crate::pipeline::DONE.to_string());
        task.set_stage(
            crate::pipeline::PAUSED,
            Some("issue_tracking hook exited 1 on `done`"),
        );
        task.save().unwrap();

        assert_row_says_resume(&repo, crate::pipeline::DONE);

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(
            task.stage(),
            crate::pipeline::DONE,
            "a task paused on `done` must go back to `done`, not `queued` or a step"
        );
        assert!(task.front.hook_paused.is_none());
        assert_eq!(
            crate::tracking::exit_code(&repo, &task, crate::pipeline::DONE),
            None,
            "the failed run must be forgotten so the hook fires again"
        );
        assert!(
            hook_rows(&repo).is_empty(),
            "the remedy the row names must clear it: {:?}",
            hook_rows(&repo)
        );
    }

    /// A one-step pipeline, so a report's routing is the only thing under test.
    fn work_pipelines() -> Pipelines {
        let yaml = "steps:\n  - id: work\n    agent: pi\n    on_pass: done\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// The same, with `blocked` declared as a staffed step of its own.
    fn staffed_pipelines() -> Pipelines {
        let yaml = "steps:\n  - id: work\n    agent: pi\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// The same, for a project whose runs stop for nobody.
    fn unattended_fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (mut repo, root_guard) = fixture(name);
        repo.config.unattended.enabled = true;
        (repo, root_guard)
    }

    fn report_outcome(repo: &Repo, pipelines: &Pipelines, id: &str, outcome: Outcome) {
        report_outcome_from(repo, pipelines, id, outcome, None)
    }

    /// The same, from a lane the dispatcher started for `started_for` — which
    /// is what turns on the stale-lane guard.
    fn report_outcome_from(
        repo: &Repo,
        pipelines: &Pipelines,
        id: &str,
        outcome: Outcome,
        started_for: Option<&str>,
    ) {
        report(
            repo,
            pipelines,
            &ReportArgs {
                task: Some(id.into()),
                stage: None,
                pass: outcome == Outcome::Pass,
                fail: outcome == Outcome::Fail,
                block: outcome == Outcome::Block,
                pause: outcome == Outcome::Pause,
                message: Some("test".into()),
                handoff: vec![],
            },
            started_for,
        )
        .unwrap();
    }

    /// A lane reports on the step it was started for, and on no other.
    ///
    /// The failure this closes is silent and total: a lane that reports twice
    /// has its second report applied to the step the first one moved the task
    /// to, which advances it again without that step ever starting a lane. Both
    /// reports say `pass`, the task reaches `done`, and the only evidence is a
    /// log file that was never written.
    ///
    /// Found in a live run, where an archivist reported twice at `document` and
    /// the step after it — `handover`, which opens the pull request and merges
    /// the plan's stack — was credited with a pass it never ran.
    #[test]
    fn a_lane_may_not_report_on_a_step_its_task_has_already_left() {
        let (repo, _root_guard) = fixture("report-from-a-stale-lane");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stale", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "stale");
        task.set_stage("work", None);
        task.save().unwrap();

        // The lane the dispatcher started: it is on `work`, and so is the task.
        report_outcome_from(&repo, &pipelines, "stale", Outcome::Pass, Some("work"));
        assert_eq!(
            queued(&repo, "stale").stage(),
            "done",
            "the first report advances the task off the step it was started for"
        );

        // The same lane, reporting a second time. The task has moved on, so
        // this report is about a step it has already left.
        let err = report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("stale".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("again".into()),
                handoff: vec![],
            },
            Some("work"),
        )
        .expect_err("a second report from one lane must be refused");
        let message = err.to_string();
        assert!(message.contains("started for `work`"), "{message}");
        assert!(message.contains("reports once"), "{message}");

        // And by hand, with no lane around it, the same command still works:
        // a person reporting is reporting on the task as it stands.
        let mut task = queued(&repo, "stale");
        task.set_stage("work", None);
        task.save().unwrap();
        report_outcome(&repo, &pipelines, "stale", Outcome::Pass);
        assert_eq!(queued(&repo, "stale").stage(), "done");
    }

    /// A lane started for `work` reports in after a person's own Escape (or
    /// the board's `p`) has already parked its task on `paused`, with
    /// `parked_from: work` and `escalated: false`. The lane never knew any of
    /// that happened — it is still mid-turn when the park lands, and its
    /// report is for the step it was actually started on.
    ///
    /// Before `parked-task-resume`, the `started_for` guard read the task's
    /// bare stage, `paused`, and refused every such report outright — bug gh
    /// group `parked-task-resume`. A late report like this is applied as if
    /// the task were still on `work`: the park is spent, and the report
    /// routes the task on exactly as it would have without the park in the
    /// way.
    #[test]
    fn a_late_report_from_a_parked_steps_own_lane_is_applied_as_if_it_never_parked() {
        let (repo, _root_guard) = fixture("late-report-onto-a-park");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "late", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "late");
        task.set_stage("work", None);
        crate::status::park(&mut task, "a person's own Escape ended the turn", false);
        task.save().unwrap();
        assert_eq!(task.stage(), "paused");
        assert_eq!(task.front.parked_from.as_deref(), Some("work"));
        assert!(!task.front.escalated);
        assert!(task.front.paused_at.is_none());

        report_outcome_from(&repo, &pipelines, "late", Outcome::Pass, Some("work"));

        let task = queued(&repo, "late");
        assert_eq!(
            task.stage(),
            "done",
            "a late report from the step it parked on must still route the task on"
        );
        assert_eq!(
            task.front.parked_from, None,
            "the park is spent by the report"
        );
        assert!(!task.front.escalated);
    }

    /// The same late report, but the park it lands on is the stop popup's
    /// own — `park_under_lock`'s `ParkedBy::Stop` road, which leaves `parked_from`
    /// set exactly like a person's own Escape does, plus `parked_by_stop`.
    /// `back_onto_its_step` already clears that mark on every ordinary road
    /// out of `paused` — see its own doc on "a stop's mark is spent by any
    /// resume, whoever sends it" — and a late report has to spend it the
    /// same way: left set, the next `spoolway start` would read
    /// `resume_stop_parked` and carry this task through a gate with nobody
    /// there to answer it.
    #[test]
    fn a_late_report_onto_a_stops_own_park_clears_its_mark_too() {
        let (repo, _root_guard) = fixture("late-report-onto-a-stop-park");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stopped", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "stopped");
        task.set_stage("work", None);
        crate::status::park(&mut task, "interrupted when dispatching stopped", false);
        task.front.parked_by_stop = true;
        task.save().unwrap();
        assert_eq!(task.stage(), "paused");
        assert_eq!(task.front.parked_from.as_deref(), Some("work"));
        assert!(task.front.parked_by_stop);

        report_outcome_from(&repo, &pipelines, "stopped", Outcome::Pass, Some("work"));

        let task = queued(&repo, "stopped");
        assert_eq!(task.stage(), "done");
        assert_eq!(task.front.parked_from, None);
        assert!(
            !task.front.parked_by_stop,
            "a late report spends a stop's mark the same way back_onto_its_step does"
        );
    }

    /// A pass that would route to a cleanup terminal is held at `blocked`
    /// while the lane's worktree still has uncommitted work in it —
    /// `auto_commit` off here, standing in for the git failure it now reports
    /// rather than swallows — so a `done` teardown cannot delete work that was
    /// never recorded (review finding 4).
    #[test]
    fn a_pass_to_a_cleanup_terminal_is_held_while_the_worktree_is_dirty() {
        let (mut repo, _root_guard) = fixture("held-dirty");
        repo.config.dispatch.auto_commit = false;
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.root.join("tracked"), "as committed").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-q", "-m", "root"]);
        add(&repo, "dirtywork", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "dirtywork");
        task.set_stage("work", None);
        task.save().unwrap();

        // A tracked file changed: work, not residue — an untracked leftover
        // alone would be swept as residue, `auto_commit` off or on.
        std::fs::write(repo.root.join("tracked"), "uncommitted").unwrap();
        let worktree = repo.root.to_str().unwrap().to_string();

        crate::platform::test_env::with_env("SPOOLWAY_WORKTREE", &worktree, || {
            report(
                &repo,
                &pipelines,
                &ReportArgs {
                    task: Some("dirtywork".into()),
                    stage: None,
                    pass: true,
                    fail: false,
                    block: false,
                    pause: false,
                    message: None,
                    handoff: vec![],
                },
                Some("work"),
            )
        })
        .unwrap();

        let task = queued(&repo, "dirtywork");
        assert_eq!(
            task.stage(),
            crate::pipeline::BLOCKED,
            "a dirty worktree must not reach a cleanup terminal"
        );
        assert_eq!(task.front.blocked_from.as_deref(), Some("work"));
        assert!(
            task.section("## Status Log")
                .unwrap_or_default()
                .contains("could not be committed"),
            "the record must say why the pass did not land"
        );
    }

    /// The whole unattended round trip through `report`: a block does not park
    /// the task at all, it puts it back on the step it blocked on with the lane
    /// that blocked marked to be continued.
    ///
    /// This is what a second, dedicated lane used to be sent to do, minus
    /// the second lane.
    #[test]
    fn an_unattended_block_resumes_the_step_instead_of_reaching_a_person() {
        let (repo, _root_guard) = unattended_fixture("unattended-roundtrip");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Block);
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "work", "a block never reaches `blocked` here");
        assert_eq!(
            task.front.resume.as_deref(),
            Some("work"),
            "the lane that blocked is continued, not replaced by a cold one"
        );
        assert_eq!(
            task.front.blocked_from, None,
            "nothing is waiting on anybody"
        );
    }

    /// A staffed `blocked` step's own pass carries the task *past* the step it
    /// blocked on, because the unblocker was asked to do that step's work and
    /// is taken at its word. `blocked` declares no `on_pass` of its own; where
    /// it goes is `blocked_from`'s `on_pass`.
    #[test]
    fn a_staffed_blocked_steps_pass_carries_the_task_past_where_it_blocked() {
        let (repo, _root_guard) = unattended_fixture("staffed-blocked-pass");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = staffed_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.front.rounds.insert("queued->work".into(), 2);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Pass);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "done",
            "the pass takes `work`'s own `on_pass`, not `work` again"
        );
        assert_eq!(
            task.front.resume, None,
            "`done` has no lane of `work`'s to continue"
        );
        assert_eq!(task.front.blocked_from, None, "nothing is blocked any more");
    }

    /// The same shape, but the step that blocked is a command step rather
    /// than an agent one. A command step is never eligible for the
    /// take-over a `--pass` from `blocked` otherwise gets: the unblocker's
    /// word is not what a `git push` or a pull request needs, so the pass
    /// hands the task back to the step itself rather than past it.
    #[test]
    fn a_staffed_blocked_steps_pass_hands_a_command_step_back_to_itself() {
        let (repo, _root_guard) = unattended_fixture("staffed-blocked-command-pass");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let yaml = "steps:\n  - id: work\n    run: 'true'\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Pass);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "work",
            "a command step is handed back to itself, never past it: {task:?}"
        );
        assert_eq!(task.front.blocked_from, None, "nothing is blocked any more");
    }

    /// A three-step forward chain, each step reached by exactly one hop, plus
    /// a staffed `blocked` — enough steps for a `--stage` naming an
    /// already-run one to land somewhere other than where it started.
    fn three_step_staffed_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
                     - id: implement\n    agent: pi\n    on_pass: review\n  \
                     - id: review\n    agent: pi\n    on_pass: look\n  \
                     - id: look\n    agent: pi\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// The mockup itself: a `--pass --stage implement` off `blocked` lands
    /// exactly on `implement`, one of the steps this task's own `steps:`
    /// says it has already run — not carried one step past it the way a
    /// plain `--pass` would be, since naming a step by hand is the
    /// unblocker choosing where this goes, not vouching for its work.
    #[test]
    fn a_staged_pass_from_blocked_lands_on_the_named_step_the_task_has_run() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-lands");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);
        let pipelines = three_step_staffed_pipelines();

        let mut task = queued(&repo, "confirm-dialog");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("confirm-dialog".into()),
                stage: Some("implement".into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("the finding is real; the fix belongs to the implementer".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap();

        assert_eq!(queued(&repo, "confirm-dialog").stage(), "implement");
    }

    /// An unblocker's own `--pass --stage` into a step that had spent its
    /// `loop:` lands there: leaving `blocked` starts every count again, so the
    /// step is not refused for the arrivals that got the task stopped.
    #[test]
    fn a_staged_pass_from_blocked_into_a_spent_step_lands_there() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-spent");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);

        let yaml = "steps:\n  \
                     - id: implement\n    agent: pi\n    loop: 1\n    on_pass: review\n  \
                     - id: review\n    agent: pi\n    on_pass: look\n  \
                     - id: look\n    agent: pi\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        let mut task = queued(&repo, "confirm-dialog");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        // `implement`'s one allowed arrival is already spent, by the cold
        // start `bank_launch` alone does not bank.
        task.front.arrivals.insert("implement".into(), 1);
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("confirm-dialog".into()),
                stage: Some("implement".into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("the finding is real; the fix belongs to the implementer".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "confirm-dialog");
        assert_eq!(task.stage(), "implement");
        assert_eq!(
            task.rounds_at("implement"),
            1,
            "the counts were reset on the way out, so this is the first arrival again"
        );
    }

    /// The other half of the mockup: naming a step this task has never run a
    /// lane at is refused, and the refusal names exactly the steps `steps:`
    /// says it has — in pipeline order, not the map's own key order.
    #[test]
    fn a_staged_pass_naming_a_step_never_run_is_refused() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-refused");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);
        let pipelines = three_step_staffed_pipelines();

        let mut task = queued(&repo, "confirm-dialog");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        task.save().unwrap();

        let err = report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("confirm-dialog".into()),
                stage: Some("handover".into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("done".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("has never been at `handover`")
                && err.contains("`implement`, `review`, `look`"),
            "{err}"
        );

        // Refused outright: the task never moved off `blocked`.
        assert_eq!(
            queued(&repo, "confirm-dialog").stage(),
            crate::pipeline::BLOCKED
        );
    }

    /// Three steps, the middle one gated — so a plain `--pass` off `blocked`,
    /// a `--stage` naming the gate itself, and a `--stage` naming a step past
    /// it can all be told apart against the same graph.
    fn gated_middle_staffed_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
                     - id: implement\n    agent: pi\n    on_pass: review\n  \
                     - id: review\n    agent: pi\n    on_pass: look\n  \
                     - id: look\n    agent: pi\n    gate: true\n    on_pass: e2e\n  \
                     - id: e2e\n    agent: pi\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// The decision the task is named for: the gate belongs to the step,
    /// whoever does its work. A task blocked at `look` — which `look`'s own
    /// `gate: true` would have held had its own lane reported the pass — is
    /// held exactly the same way once an unblocker's `--pass` stands in for
    /// that step instead, on `paused` at `look`'s own gate rather than
    /// carried on to `e2e`. A later `spoolway resume` takes `look`'s own
    /// `on_pass`, not `look` itself — the unblocker's pass is still taken at
    /// its word, the same as an ungated one would be.
    #[test]
    fn a_pass_from_blocked_is_held_at_the_origins_own_gate() {
        let (repo, _root_guard) = unattended_fixture("blocked-pass-origin-gate");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "tab-shell", &[]);
        let pipelines = gated_middle_staffed_pipelines();

        let mut task = queued(&repo, "tab-shell");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "tab-shell", Outcome::Pass);

        let task = queued(&repo, "tab-shell");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("look"));
        assert_eq!(task.front.paused_by.as_deref(), Some("gate"));
        assert_eq!(
            task.front.blocked_from, None,
            "cleared the same way an ordinary gate hold leaves it — nothing here is a \
             caught block for `spoolway resume` to hand back to `blocked`"
        );

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "tab-shell".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "tab-shell").stage(),
            "e2e",
            "`look`'s own `on_pass`, not `look` itself — the unblocker's pass still stands \
             in for finished work"
        );
    }

    /// A `run:` step has no agent work for an unblocker to stand in for, so a
    /// `--pass` from `blocked` hands it straight back to itself, gated or not.
    /// A gated command that failed and blocked is therefore run again, not
    /// held on `paused` as though it had exited 0, and a later `spoolway
    /// resume` has no `on_pass` of it to take.
    #[test]
    fn a_pass_from_blocked_runs_a_gated_command_step_again() {
        let (repo, _root_guard) = unattended_fixture("blocked-pass-gated-command");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "tab-shell", &[]);
        let pipelines = gated_deploy_pipelines();

        let mut task = queued(&repo, "tab-shell");
        task.bank_launch(crate::pipeline::QUEUED, "deploy");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("deploy".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "tab-shell", Outcome::Pass);

        let task = queued(&repo, "tab-shell");
        assert_eq!(
            task.stage(),
            "deploy",
            "the command never exited 0, so the unblocker's pass hands it back to itself"
        );
        assert_eq!(
            task.front.paused_by, None,
            "no gate holds a pass nobody gave"
        );
        assert_eq!(task.front.paused_at, None);
    }

    /// A pipeline whose `deploy` is a gated `run:` step that goes on to `done`
    /// on a pass, with `blocked` staffed.
    fn gated_deploy_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
                     - id: deploy\n    run: 'exit 1'\n    gate: true\n    on_pass: done\n  \
                     - id: blocked\n    agent: pi\n    session: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// A task paused at `deploy`'s gate with `last_report` filed from
    /// `reported_from`: the dispatcher's own hold files it from `deploy`, and
    /// a release that held an unblocker's pass filed it from `blocked`.
    fn held_at_deploy(repo: &Repo, reported_from: &str) -> Task {
        add(repo, "tab-shell", &[]);
        let mut task = queued(repo, "tab-shell");
        task.set_stage(crate::pipeline::PAUSED, None);
        task.front.paused_at = Some("deploy".into());
        task.front.paused_by = Some(Gate::Step.as_str().to_string());
        task.front.last_report = Some(crate::task::LastReport {
            step: reported_from.into(),
            outcome: "pass".into(),
            at: 0,
            blocked: false,
        });
        task.save().unwrap();
        task
    }

    /// A task an earlier release left held at a gated command step, from an
    /// unblocker's pass, is resumed back onto the step rather than down its
    /// `on_pass`: the command never exited 0. `resume_road`, which `queue
    /// route` and the board's `(next)` row read, names the step too. Fails
    /// without the `command_pass_handed_back` branch of `resume_road`, which
    /// sends it to `done`.
    #[test]
    fn a_gated_command_pass_an_unblocker_gave_is_resumed_onto_the_step() {
        let (repo, _root_guard) = unattended_fixture("held-unblocker-command-pass");
        let pipelines = gated_deploy_pipelines();
        let task = held_at_deploy(&repo, crate::pipeline::BLOCKED);

        assert_eq!(
            resume_road(&task, &pipelines).unwrap().destination(),
            "deploy"
        );

        resume(&repo, &pipelines, &resume_args("tab-shell", None), None).unwrap();
        let task = queued(&repo, "tab-shell");
        assert_eq!(task.stage(), "deploy");
        assert_eq!(task.front.paused_at, None);
        assert_eq!(task.front.paused_by, None);
    }

    /// The dispatcher parking a gated command after its own exit 0 files the
    /// report from the command step itself, and that pass still takes
    /// `on_pass` on resume.
    #[test]
    fn a_gated_command_pass_the_command_gave_still_takes_on_pass() {
        let (repo, _root_guard) = unattended_fixture("held-command-own-pass");
        let pipelines = gated_deploy_pipelines();
        let task = held_at_deploy(&repo, "deploy");

        assert_eq!(
            resume_road(&task, &pipelines).unwrap().destination(),
            "done"
        );
    }

    /// Blocks `tab-shell` at `stopped` on `gated_middle_staffed_pipelines`,
    /// after banking `banked` (each `(from, to)` in order), and returns the
    /// error a `--pass --stage <stage>` from `blocked` is refused with —
    /// asserting nothing was written.
    fn staged_pass_refusal(
        name: &str,
        stopped: &str,
        banked: &[(&str, &str)],
        stage: &str,
    ) -> String {
        let (repo, _root_guard) = unattended_fixture(name);
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "tab-shell", &[]);
        let pipelines = gated_middle_staffed_pipelines();

        let mut task = queued(&repo, "tab-shell");
        for (from, to) in banked {
            task.bank_launch(from, to);
        }
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some(stopped.into());
        task.save().unwrap();

        let err = report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("tab-shell".into()),
                stage: Some(stage.into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("done".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap_err();
        assert_eq!(queued(&repo, "tab-shell").stage(), crate::pipeline::BLOCKED);
        format!("{err:#}")
    }

    /// The other half: `--stage` may not carry the task past a gate it never
    /// answered. Naming `e2e`, past `look`'s own gate, is refused by name,
    /// and the steps it offers instead stop at the gate — `e2e` has been run,
    /// but offering it back would contradict the refusal.
    #[test]
    fn a_staged_pass_from_blocked_may_not_name_a_step_past_a_gate() {
        let err = staged_pass_refusal(
            "staged-pass-past-a-gate",
            "look",
            &[
                (crate::pipeline::QUEUED, "implement"),
                ("implement", "review"),
                ("review", "look"),
                ("look", "e2e"),
            ],
            "e2e",
        );
        assert!(err.contains("stopped at `look`, which is gated"), "{err}");
        assert!(
            err.ends_with("Name `look` or a step before it: `implement`, `review`, `look`"),
            "{err}"
        );
    }

    /// A task that has run nothing at or before the gate has no step to offer
    /// back, and the refusal says so rather than ending on its own colon.
    #[test]
    fn a_staged_pass_refusal_with_no_step_to_name_does_not_end_on_an_empty_list() {
        let err = staged_pass_refusal("staged-pass-empty-list", "look", &[], "e2e");
        assert!(err.contains("stopped at `look`, which is gated"), "{err}");
        assert!(
            err.ends_with(
                "This task has not run `look` or any step before it, so there is none to name — report a plain `--pass` without `--stage` instead"
            ),
            "{err}"
        );
    }

    /// A report filed from `queued` is refused for what it is, not as a step
    /// name that is not in the pipeline.
    #[test]
    fn a_report_from_queued_says_no_lane_works_there() {
        let pipelines = gated_middle_staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        let (repo, _root_guard) = unattended_fixture("report-from-queued");
        add(&repo, "tab-shell", &[]);
        let mut task = queued(&repo, "tab-shell");
        let err = route(
            &mut task,
            pipeline,
            crate::pipeline::QUEUED,
            Outcome::Pass,
            false,
            None,
            0,
        )
        .err()
        .expect("a report from queued is refused");
        let err = format!("{err:#}");
        assert!(err.contains("which no lane works at"), "{err}");
        assert!(!err.contains("is not a step"), "{err}");
        assert!(err.contains("starts on its own"), "{err}");

        task.set_stage(crate::pipeline::PAUSED, None);
        let err = route(
            &mut task,
            pipeline,
            crate::pipeline::PAUSED,
            Outcome::Pass,
            false,
            None,
            0,
        )
        .err()
        .expect("a report from paused is refused");
        let err = format!("{err:#}");
        assert!(err.contains("`spoolway resume tab-shell`"), "{err}");
    }

    /// A step past the gate that this task never ran is still refused for
    /// the gate, not for being unrun — the gate is what is in the way.
    #[test]
    fn a_staged_pass_past_a_gate_to_an_unrun_step_names_the_gate() {
        let err = staged_pass_refusal(
            "staged-pass-past-a-gate-unrun",
            "look",
            &[
                (crate::pipeline::QUEUED, "implement"),
                ("implement", "review"),
                ("review", "look"),
            ],
            "e2e",
        );
        assert!(!err.contains("has never been at"), "{err}");
        assert_eq!(
            err,
            "task `tab-shell` stopped at `look`, which is gated - a pass from `blocked` may \
             not name a step past it. Name `look` or a step before it: `implement`, `review`, \
             `look`"
        );
    }

    /// Stopped short of the gate: the refusal names the gate further on as
    /// the gated step, not the step the task stopped at.
    #[test]
    fn a_staged_pass_stopped_short_of_the_gate_names_the_gate_after_it() {
        let err = staged_pass_refusal(
            "staged-pass-short-of-a-gate",
            "review",
            &[
                (crate::pipeline::QUEUED, "implement"),
                ("implement", "review"),
                ("review", "look"),
                ("look", "e2e"),
            ],
            "e2e",
        );
        assert!(
            err.starts_with("task `tab-shell` stopped at `review`, and `look` after it is gated"),
            "{err}"
        );
        assert!(
            err.ends_with("Name `look` or a step before it: `implement`, `review`, `look`"),
            "{err}"
        );
    }

    /// Naming the gated step itself is not naming a step past it — it runs
    /// again, and its own gate holds its own pass, same as any other visit.
    #[test]
    fn a_staged_pass_may_name_the_gated_step_itself() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-names-the-gate");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "tab-shell", &[]);
        let pipelines = gated_middle_staffed_pipelines();

        let mut task = queued(&repo, "tab-shell");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("tab-shell".into()),
                stage: Some("look".into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("send it back for another look".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "tab-shell");
        assert_eq!(
            task.stage(),
            "look",
            "staged onto the gate step to run again, not held in front of it — the person \
             asked for this by name"
        );
        assert_eq!(task.front.paused_at, None);
    }

    /// `--stage` is bounded to `--pass` off `blocked` itself, the same as
    /// `--pause` — named and refused rather than silently ignored off any
    /// other step.
    #[test]
    fn a_staged_pass_is_refused_off_blocked() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-off-blocked");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);
        let pipelines = three_step_staffed_pipelines();

        let mut task = queued(&repo, "confirm-dialog");
        task.set_stage("implement", None);
        task.save().unwrap();

        let err = report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("confirm-dialog".into()),
                stage: Some("review".into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("done".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("--stage only means anything on a `--pass` from `blocked`"),
            "{err}"
        );
        assert_eq!(queued(&repo, "confirm-dialog").stage(), "implement");
    }

    /// A staffed `blocked` banks its own arrival route the moment an
    /// unblocker's lane starts, the same as any other step — so on a real
    /// unattended run, `blocked` itself sits in `steps:` by the time
    /// `--stage` could ever be typed at all. `steps_run` excludes it
    /// anyway: naming `blocked` is refused, never accepted as a step this
    /// task has run, because a `--stage blocked` would clear `blocked_from`
    /// and leave the *next* plain `--pass` from `blocked` with nothing to
    /// carry it but the pipeline's own entry — restarting a task that may
    /// already have pushed a branch or opened a pull request.
    #[test]
    fn a_staged_pass_naming_blocked_itself_is_refused_even_though_blocked_banked_its_own_launch() {
        let (repo, _root_guard) = unattended_fixture("staged-pass-excludes-blocked");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);
        let pipelines = three_step_staffed_pipelines();

        let mut task = queued(&repo, "confirm-dialog");
        task.bank_launch(crate::pipeline::QUEUED, "implement");
        task.bank_launch("implement", "review");
        task.bank_launch("review", "look");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("look".into());
        // The staffed unblocker's own lane banking its own arrival — exactly
        // what `finish_launch_bookkeeping` does for any staffed step, `blocked` included.
        task.bank_launch("look", crate::pipeline::BLOCKED);
        task.save().unwrap();

        let err = report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("confirm-dialog".into()),
                stage: Some(crate::pipeline::BLOCKED.into()),
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("done".into()),
                handoff: vec![],
            },
            None,
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("has never been at `blocked`"),
            "`blocked` must be refused as a `--stage` target even though it is in `steps:`: {err}"
        );
        assert!(
            !err.contains("`blocked`,") && !err.contains(", `blocked`"),
            "`blocked` must not be named among the steps this task has run: {err}"
        );
        assert!(
            err.contains("`implement`, `review`, `look`"),
            "the three real steps should still be named: {err}"
        );
        assert_eq!(
            queued(&repo, "confirm-dialog").stage(),
            crate::pipeline::BLOCKED
        );
    }

    /// A `--block` reported from `blocked` parks on `paused` first (see
    /// `a_staffed_blocked_steps_fail_block_or_pause_lands_on_paused`), and
    /// resuming it hands the task back to the step it blocked on rather than
    /// past it: `past_the_gate`'s own `cleared_block` branch always passes
    /// `takes_over: false` to `cleared_block_target`, because a `--block`
    /// never claims the step's work is done the way a `--pass` from
    /// `blocked` does. The lane that blocked is continued, not replaced by a
    /// cold one, the same as a take-over would leave it.
    #[test]
    fn a_block_reported_from_blocked_hands_the_task_back_to_where_it_blocked_once_resumed() {
        let (repo, _root_guard) = fixture("blocked-hands-back-block");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = staffed_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Block);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            crate::pipeline::PAUSED,
            "parks for a person first"
        );

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "work",
            "a block is never taken as the unblocker's word for the step's work — it goes \
             back, never past it"
        );
        assert_eq!(
            task.front.resume.as_deref(),
            Some("work"),
            "the lane that blocked is continued, not replaced by a cold one"
        );
        assert_eq!(task.front.blocked_from, None, "nothing is blocked any more");
        let log = task.section("## Status Log").unwrap();
        assert!(
            log.contains("block cleared by hand; nothing was done at `work`"),
            "{log}"
        );
    }

    /// The same, for `--pause` — the one outcome from `blocked` the old tests
    /// never covered here, since a `--pause` from anywhere but `blocked` did
    /// not exist before this rule.
    #[test]
    fn a_pause_reported_from_blocked_also_hands_the_task_back_to_where_it_blocked_once_resumed() {
        let (repo, _root_guard) = fixture("blocked-hands-back-pause");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = staffed_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Pause);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            crate::pipeline::PAUSED,
            "parks for a person first"
        );

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "work",
            "nothing here could clear it, so a person's resume hands it back rather than \
             carrying it past"
        );
        assert_eq!(
            task.front.resume.as_deref(),
            Some("work"),
            "the lane that blocked is continued, not replaced by a cold one"
        );
        assert_eq!(task.front.blocked_from, None, "nothing is blocked any more");
    }

    /// A task whose `blocked_from` was lost does not restart at the pipeline's
    /// entry. That fallback is what sent three of four tasks in one run back to
    /// `implement` to re-walk a pipeline nothing had questioned; `last_report`
    /// is consulted first, and it knows where the task actually was.
    #[test]
    fn a_lost_origin_falls_back_to_the_last_report_not_the_entry() {
        let (repo, _root_guard) = unattended_fixture("lost-origin");
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = None;
        task.front.last_report = Some(crate::task::LastReport {
            step: "work".into(),
            outcome: "block".into(),
            at: 1,
            blocked: false,
        });

        assert_eq!(
            resume_target(&task, pipeline),
            "work",
            "the last step that reported beats the entry"
        );

        // A task that ran but never reported has nothing better than the
        // entry; one that never ran at all goes back to `queued`, where the
        // dependency and hook gates apply again (jobs review finding 1).
        task.front.last_report = None;
        task.front.worktree_path = Some("/tmp/spoolway-fake-worktree".into());
        assert_eq!(
            resume_target(&task, pipeline),
            pipeline.entry(),
            "a task that never reported has nothing better than the entry"
        );
        task.front.worktree_path = None;
        assert_eq!(
            resume_target(&task, pipeline),
            crate::pipeline::QUEUED,
            "a task that never started is gated again, not launched off the entry"
        );
    }

    /// `last_report` naming `blocked` is no fallback at all. A staffed
    /// `blocked` step reports like any other, so once its lane has been round
    /// once that is what `last_report` holds — and an origin of `blocked` is
    /// the one `set_blocked_from` refuses to write, because `blocked` has no
    /// `on_pass` for `cleared_block_target` to read. Taking it would hand the
    /// task back to `blocked` on every pass, for ever. The entry is a worse
    /// answer than a real origin and a far better one than a loop.
    #[test]
    fn a_last_report_naming_blocked_is_skipped_rather_than_resumed_into() {
        let (repo, _root_guard) = unattended_fixture("blocked-last-report");
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = None;
        task.front.last_report = Some(crate::task::LastReport {
            step: crate::pipeline::BLOCKED.into(),
            outcome: "block".into(),
            at: 1,
            blocked: false,
        });

        assert_eq!(
            resume_target(&task, pipeline),
            pipeline.entry(),
            "`blocked` is the one origin there is no way back from"
        );
        assert_ne!(
            cleared_block_target(&task, pipeline, true),
            crate::pipeline::BLOCKED,
            "so clearing the block never hands the task back to `blocked`"
        );

        task.front.blocked_from = Some(crate::pipeline::BLOCKED.into());
        assert_eq!(
            resume_target(&task, pipeline),
            pipeline.entry(),
            "an old task file carrying `blocked_from: blocked` is refused the same way"
        );
    }

    /// A task blocked on a step its pipeline has since renamed has nothing
    /// to resume at but the entry, and the entry would redo passed work. A
    /// plain resume is refused, naming the step and `--stage`. Naming a step
    /// by hand still rescues it. A task that ran but never reported keeps
    /// resuming at the entry as it always did.
    #[test]
    fn a_plain_resume_of_a_task_on_a_renamed_step_is_refused_and_stage_rescues_it() {
        let (repo, _root_guard) = unattended_fixture("renamed-step");
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("old-name".into());
        task.front.last_report = Some(crate::task::LastReport {
            step: "older-name".into(),
            outcome: "block".into(),
            at: 1,
            blocked: false,
        });
        task.save().unwrap();

        let err = resume(&repo, &pipelines, &resume_args("stuck", None), None)
            .expect_err("a step the pipeline dropped is not a resume target");
        let said = format!("{err:#}");
        assert!(
            said.contains("old-name") && said.contains("--stage"),
            "{said}"
        );
        assert_eq!(queued(&repo, "stuck").stage(), crate::pipeline::BLOCKED);

        resume(
            &repo,
            &pipelines,
            &resume_args("stuck", Some(pipeline.entry())),
            None,
        )
        .expect("--stage still rescues it");
        assert_eq!(queued(&repo, "stuck").stage(), pipeline.entry());

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = None;
        task.front.last_report = None;
        task.front.worktree_path = Some("/tmp/spoolway-fake-worktree".into());
        match resume_road(&task, &pipelines).expect("nothing recorded, nothing stranded") {
            ResumeRoad::Step(step) => assert_eq!(step, pipeline.entry()),
            _ => panic!("a task that ran but never reported resumes at the entry"),
        }
    }

    /// Every way a task can stand on a step its pipeline renamed away is
    /// refused by a plain resume with the same message, naming the step and
    /// `--stage`: running there (`stage:`), parked there, and held at its
    /// gate. Before, the first landed on the entry, the second wrote the
    /// dead step back as its stage, and the third never named `--stage`.
    #[test]
    fn a_plain_resume_is_refused_for_a_stage_a_park_and_a_gate_on_a_dropped_step() {
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        for case in ["stage", "parked", "paused"] {
            let (repo, _root_guard) = unattended_fixture(&format!("dropped-{case}"));
            add(&repo, "stuck", &[]);
            let mut task = queued(&repo, "stuck");
            task.front.worktree_path = Some("/tmp/spoolway-fake-worktree".into());
            match case {
                "stage" => task.set_stage("old-name", None),
                "parked" => {
                    task.set_stage(crate::pipeline::PAUSED, None);
                    task.front.parked_from = Some("old-name".into());
                }
                _ => {
                    task.set_stage(crate::pipeline::PAUSED, None);
                    task.front.paused_at = Some("old-name".into());
                }
            }
            task.save().unwrap();

            let err = resume(&repo, &pipelines, &resume_args("stuck", None), None)
                .expect_err("a step the pipeline dropped is not a resume target");
            let said = format!("{err:#}");
            assert!(
                said.contains("old-name") && said.contains("--stage"),
                "{case}: {said}"
            );
            assert_eq!(queued(&repo, "stuck").stage(), task.stage(), "{case}");

            resume(
                &repo,
                &pipelines,
                &resume_args("stuck", Some(pipeline.entry())),
                None,
            )
            .unwrap_or_else(|err| panic!("{case}: --stage should rescue it: {err:#}"));
            assert_eq!(queued(&repo, "stuck").stage(), pipeline.entry(), "{case}");
        }
    }

    /// `started` is a hook event, not a reserved stage, so a pipeline may name
    /// a step that. A task parked on one that was then renamed is stranded
    /// like any other, and a plain resume refuses it rather than writing the
    /// dead step back as its stage.
    #[test]
    fn a_plain_resume_is_refused_for_a_park_on_a_dropped_step_named_started() {
        let (repo, _root_guard) = unattended_fixture("dropped-started");
        let pipelines = staffed_pipelines();
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::PAUSED, None);
        task.front.parked_from = Some(crate::pipeline::STARTED.into());
        task.save().unwrap();

        let err = resume(&repo, &pipelines, &resume_args("stuck", None), None)
            .expect_err("a step the pipeline dropped is not a resume target");
        let said = format!("{err:#}");
        assert!(
            said.contains("`started`") && said.contains("--stage"),
            "{said}"
        );
        assert_eq!(queued(&repo, "stuck").stage(), crate::pipeline::PAUSED);
    }

    /// A fail, a block or a pause from that same staffed step never lands back
    /// on `blocked` itself any more — there is no escalation past it, so it
    /// parks on `paused` instead, naming the step it originally blocked on and
    /// keeping `blocked_from` for a later resume to read.
    #[test]
    fn a_staffed_blocked_steps_fail_block_or_pause_lands_on_paused() {
        for outcome in [Outcome::Fail, Outcome::Block, Outcome::Pause] {
            let (repo, _root_guard) = unattended_fixture(&format!("staffed-blocked-{outcome}"));
            let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
            git(&["config", "user.email", "t@example.com"]);
            git(&["config", "user.name", "t"]);
            git(&["commit", "-q", "--allow-empty", "-m", "root"]);
            add(&repo, "stuck", &[]);
            let pipelines = staffed_pipelines();

            let mut task = queued(&repo, "stuck");
            task.set_stage(crate::pipeline::BLOCKED, None);
            task.front.blocked_from = Some("work".into());
            task.save().unwrap();

            report_outcome(&repo, &pipelines, "stuck", outcome);
            let task = queued(&repo, "stuck");
            assert_eq!(task.stage(), crate::pipeline::PAUSED, "{outcome}");
            assert_eq!(
                task.front.paused_at.as_deref(),
                Some("work"),
                "{outcome}: paused_at names the step it originally blocked on"
            );
            assert_eq!(
                task.front.blocked_from.as_deref(),
                Some("work"),
                "{outcome}: the origin survives, for a resume to know which step to hand the \
                 task back to"
            );
        }
    }

    /// A pause `commands::report` raises from `blocked` itself leaves the very
    /// same `blocked_from == paused_at` shape a schedule's own caught block at
    /// that step would — the ambiguity `commands::caught_at` exists to
    /// resolve, by reading `last_report.step` instead: `blocked`'s own road
    /// files its report from `blocked`, never from the step `paused_at`
    /// names, so `caught_at` reads `None` and `spoolway resume` still reaches
    /// `cleared_block_target` for it. The newer hand-back rule passes
    /// `takes_over: false`, because the failed report did not claim that the
    /// blocked step's work was done.
    #[test]
    fn resuming_a_pause_raised_from_blocked_itself_still_clears_the_block() {
        let (repo, _root_guard) = fixture("blocked-pause-resume");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = staffed_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("work"));
        assert_eq!(task.front.blocked_from.as_deref(), Some("work"));

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "stuck").stage(),
            "work",
            "a pause raised from `blocked` itself still reaches cleared_block_target, \
             handing the task back to its blocked step rather than treating the failed report \
             as completed work"
        );
    }

    /// `--pause` only means anything on `blocked` itself — every other step
    /// still has `--fail` and `--block` to say the same thing, so this is
    /// refused rather than silently read as one of them.
    #[test]
    fn a_pause_reported_anywhere_but_blocked_is_refused() {
        let (repo, _root_guard) = fixture("pause-elsewhere");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.save().unwrap();

        let err = report(
            &repo,
            &Pipelines::builtin(),
            &ReportArgs {
                task: Some("stuck".into()),
                stage: None,
                pass: false,
                fail: false,
                block: false,
                pause: true,
                message: Some("waiting on the owner".into()),
                handoff: vec![],
            },
            None,
        )
        .expect_err("--pause anywhere but `blocked` must be refused");
        let message = err.to_string();
        assert!(message.contains("blocked"), "{message}");
        assert_eq!(
            queued(&repo, "stuck").stage(),
            "implement",
            "a refused report must not have moved the task"
        );
    }

    /// Every road to `blocked` resumes, not only an explicit `--block`. A fail
    /// that falls through is the same fact about the task — it is not moving
    /// without help — and with nobody to fetch, the only help available is
    /// another go by the lane that knows what happened.
    #[test]
    fn an_unattended_fail_that_falls_through_to_blocked_resumes_too() {
        let (repo, _root_guard) = unattended_fixture("unattended-fail");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = work_pipelines();

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        assert_eq!(queued(&repo, "stuck").stage(), "work");
    }

    /// A spent budget is a request for a person — its exit is `blocked` from
    /// every step — on a pipeline that does not stage `blocked` itself, which
    /// is what this test strips out of the shipped set. Unattended there is no
    /// person to hand the task to, only the run's own resume straight back
    /// onto `review` — which hands nothing back, so the same wall would be hit
    /// on every transition after this one. The bound is skipped outright
    /// instead, and the task file does not fill up with rounds bought against
    /// a wall nothing can clear.
    #[test]
    fn a_budget_bound_for_a_person_does_not_bind_an_unattended_run_with_no_staffed_blocked_step() {
        let (repo, _root_guard) = unattended_fixture("unattended-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // The shipped `default` pipeline, minus its `blocked` step — the case
        // this test is about, and the shape every project's pipeline had
        // before this one could declare it.
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
        }

        // Spent right up to `implement`'s own limit: attended, the next
        // failure is where the task stops.
        let spent = implement_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.arrivals.insert("implement".into(), spent);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "implement",
            "the loop goes round rather than stopping"
        );
        assert_eq!(
            task.rounds_at("implement"),
            spent + 1,
            "the bound is skipped outright, not spent and refunded, so this move banks an \
             arrival exactly as an unbounded one would"
        );
    }

    /// A spent `loop:` escalates to `blocked`, and the lane staffing `blocked`
    /// clearing the task is the end of that escalation: its pass lands on the
    /// step it was cleared for, not on `paused`, and every arrival count
    /// starts again from the one that pass makes.
    #[test]
    fn an_unblockers_pass_lands_on_the_step_whose_loop_was_spent() {
        let (repo, _root_guard) = unattended_fixture("unblocker-pass-spent-loop");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "spent", &[]);

        let yaml = "steps:\n  \
                    - id: review\n    agent: pi\n    on_pass: e2e\n  \
                    - id: e2e\n    agent: pi\n    loop: 2\n    on_pass: done\n  \
                    - id: blocked\n    agent: pi\n    session: true\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );

        let mut task = queued(&repo, "spent");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("review".into());
        task.front.arrivals.insert("e2e".into(), 2);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "spent", Outcome::Pass);

        let task = queued(&repo, "spent");
        assert_eq!(
            task.stage(),
            "e2e",
            "the unblocker cleared this task; spoolway's own counter must not park it"
        );
        assert_eq!(
            task.rounds_at("e2e"),
            1,
            "the count starts again at the arrival"
        );
    }

    /// The unblocker's own `--block` parks the task on `paused`, but that is
    /// still leaving `blocked`: every count starts again there, so the
    /// person's resume goes back to `review` and the pass on to the
    /// once-spent `e2e` lands instead of being refused.
    #[test]
    fn an_unblockers_block_resets_the_counts_before_a_person_resumes() {
        let (repo, _root_guard) = unattended_fixture("unblocker-block-spent-loop");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "spent", &[]);

        let yaml = "steps:\n  \
                    - id: review\n    agent: pi\n    on_pass: e2e\n  \
                    - id: e2e\n    agent: pi\n    loop: 2\n    on_pass: done\n  \
                    - id: blocked\n    agent: pi\n    session: true\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );

        let mut task = queued(&repo, "spent");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("review".into());
        task.front.arrivals.insert("e2e".into(), 2);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "spent", Outcome::Block);
        let task = queued(&repo, "spent");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.rounds_at("e2e"), 0, "parking is leaving `blocked`");

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "spent".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(queued(&repo, "spent").stage(), "review");

        report_outcome(&repo, &pipelines, "spent", Outcome::Pass);
        assert_eq!(queued(&repo, "spent").stage(), "e2e");
    }

    /// The same spent budget, on the shipped pipeline as it actually ships —
    /// staffing `blocked` with its own sample prompt. The exit is a real
    /// destination again, staffed by a lane rather than a person, and the
    /// limit binds exactly as it would in an attended run.
    #[test]
    fn a_budget_bound_does_bind_an_unattended_run_that_staffs_blocked() {
        let (repo, _root_guard) = unattended_fixture("unattended-rounds-staffed");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let pipelines = Pipelines::builtin();

        let spent = implement_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.arrivals.insert("implement".into(), spent);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            crate::pipeline::BLOCKED,
            "a lane is staffed there now, so the budget really does stop the loop"
        );
    }

    /// The same pipeline, the same spent budget, attended: the limit is
    /// exactly what it always was. The mode is the only difference between
    /// these two.
    #[test]
    fn a_budget_bound_for_a_person_still_binds_an_attended_run() {
        let (repo, _root_guard) = fixture("attended-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let pipelines = Pipelines::builtin();

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front
            .arrivals
            .insert("implement".into(), implement_limit(&pipelines));
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        assert_eq!(queued(&repo, "stuck").stage(), "blocked");
    }

    /// The whole of what a budget counts, walked end to end on the pipeline
    /// that ships: `implement` carries `loop: 2`, counting every arrival by
    /// any route — the cold start included — so it is `review`'s *second*
    /// failure that takes the exit, not its third. Under the old, retired
    /// rule a *passing* `implement` was redirected before `review` ever saw
    /// the fix it had just reported on; this pipeline accepts that trade the
    /// other way now, on the steps a pass and a fail both reach.
    #[test]
    fn the_second_failure_takes_the_exit_now_the_cold_start_counts() {
        let (repo, _root_guard) = fixture("two-arrivals");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "spinner", &[]);

        let pipelines = Pipelines::builtin();
        assert_eq!(
            implement_limit(&pipelines),
            2,
            "the shipped budget this walks lap by lap"
        );

        let mut task = queued(&repo, "spinner");
        task.set_stage("implement", None);
        task.save().unwrap();

        // The cold start already banked one arrival, so `implement` may take
        // exactly one more: the first failure's retry lands.
        report_outcome(&repo, &pipelines, "spinner", Outcome::Pass);
        assert_eq!(queued(&repo, "spinner").stage(), "review");
        report_outcome(&repo, &pipelines, "spinner", Outcome::Fail);
        let task = queued(&repo, "spinner");
        assert_eq!(task.stage(), "implement", "the first retry still lands");
        assert!(
            !task.front.last_report.as_ref().unwrap().blocked,
            "a retry that lands is not a block"
        );
        assert_eq!(task.rounds_at("implement"), 2);

        // The budget is spent now, so the second failure is the one over —
        // one hop earlier than under the retired per-route count.
        report_outcome(&repo, &pipelines, "spinner", Outcome::Pass);
        assert_eq!(queued(&repo, "spinner").stage(), "review");
        report_outcome(&repo, &pipelines, "spinner", Outcome::Fail);
        let task = queued(&repo, "spinner");
        assert_eq!(
            task.stage(),
            crate::pipeline::BLOCKED,
            "a spent budget parks on `blocked`, never back on `implement`"
        );
        let left = task.front.last_report.as_ref().unwrap();
        assert_eq!(left.outcome, "fail", "the lane's own verdict is untouched");
        assert!(
            left.blocked,
            "the fail the spent loop sent to `blocked` is left marked for the ledger"
        );
        let log = task.section("## Status Log").unwrap_or_default();
        assert!(
            log.contains(
                "`review` may not send this to `implement` a 3rd time — `implement` has \
                 `loop: 2`; carrying on to `blocked`"
            ),
            "{log}"
        );
    }

    /// A pass reported from `blocked` starts every step's `loop:` count again:
    /// the spent limit is what sent the task there, and carrying on past it
    /// into the same wall would only park it. The reset reaches the saved file
    /// too — `Task::parse` backfills `arrivals` from `rounds`, so the old
    /// counts must not come back on the re-read `queued` does.
    #[test]
    fn a_pass_reported_from_blocked_resets_the_arrivals() {
        let (repo, _root_guard) = fixture("blocked-pass-keeps-arrivals");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = Pipelines::builtin();
        let spent = implement_limit(&pipelines);

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.arrivals.insert("implement".into(), spent);
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Pass);

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "document",
            "the pass takes `review`'s own `on_pass`, past the step that blocked"
        );
        assert_eq!(
            task.rounds_at("implement"),
            0,
            "leaving `blocked` starts every count again"
        );
        // Only the one lap this move itself banked: the history before it is gone.
        assert_eq!(task.front.rounds.len(), 1, "{:?}", task.front.rounds);
    }

    /// And the same for the other self-resume: an unattended run with nobody
    /// staffing `blocked` sends the task straight back to the step it stopped
    /// on, and that road hands nothing back either. Blocked below the limit
    /// here on purpose — a *spent* budget never reaches this road at all, by
    /// `apply_loop_budget`'s own carve-out — so what is under test is the
    /// refund, not the escalation.
    #[test]
    fn the_unattended_self_resume_leaves_the_rounds_spent() {
        let (repo, _root_guard) = unattended_fixture("unattended-resume-keeps-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // Unstaffed, so the block is answered by the run itself.
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
        }

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.rounds.insert("review->implement".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Block);

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "review",
            "the run resumed it where it stopped"
        );
        assert_eq!(
            task.rounds_via("review", "implement"),
            1,
            "nobody decided this loop deserved another go, so its one spent round stands"
        );
    }

    /// `gate:` only turns a *pass* into `paused` — a `--block` from a gated
    /// step is still an ordinary block, and unattended it resumes like any
    /// other: there is nobody to park in front of.
    #[test]
    fn a_gated_step_blocks_like_any_other_when_nobody_is_there() {
        let (repo, _root_guard) = unattended_fixture("unattended-gate");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let yaml = "steps:\n  \
             - id: work\n    agent: pi\n    gate: true\n    on_pass: done\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Block);
        assert_eq!(queued(&repo, "stuck").stage(), "work");
    }

    /// And attended, the same gated step parks in front of the person it is
    /// written for.
    #[test]
    fn a_gated_step_parks_for_the_person_it_is_written_for() {
        let (repo, _root_guard) = fixture("attended-gate");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let yaml = "steps:\n  \
             - id: work\n    agent: pi\n    gate: true\n    on_pass: done\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Block);
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "blocked");
        assert_eq!(task.front.blocked_from.as_deref(), Some("work"));
    }

    /// A pipeline for one bounded loop — `work` fails to `retry`, `retry`
    /// passes back — with `retry`'s own arrival limit spelled out. The budget
    /// sits on `retry` because `retry` is what `work` arrives back at; the
    /// counter it reads is `retry`'s own `arrivals`. Where the spent budget
    /// lands is not the file's to say: it is `blocked`.
    fn looping_pipelines(limit: u32) -> Pipelines {
        let yaml = format!(
            "steps:\n  \
             - id: work\n    agent: pi\n    \
             on_pass: ship\n    on_fail: retry\n  \
             - id: retry\n    agent: pi\n    loop: {limit}\n    on_pass: work\n  \
             - id: ship\n    run: x\n    on_pass: done\n"
        );
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", &yaml).unwrap(),
        );
        pipelines
    }

    /// The whole point of a loop budget: a loop that will not converge stops,
    /// and stops on `blocked`, where a person reads what it could not settle.
    /// `work`'s own `on_pass` is `ship`, and a spent budget does not take it —
    /// which is the change this replaced a per-step `on_loop_max:` with.
    #[test]
    fn a_spent_budget_parks_the_task_on_blocked() {
        let (repo, _root_guard) = fixture("on-max-routes");
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(1);

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.arrivals.insert("retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("work"),
            "a person picking this up resumes at the step that could not settle it"
        );
        assert!(
            task.section("## Status Log").unwrap_or_default().contains(
                "`work` may not send this to `retry` a 2nd time — `retry` has `loop: 1`; \
                 carrying on to `blocked`"
            ),
            "the move it refused, and what carried it on, have to travel with it"
        );
    }

    /// A resumed lane is still a prompt, but sending the task back is what a
    /// lap costs now — so a loop on a `session:` step is bounded by how many
    /// times it sends work back, not by how many of the lanes that did so
    /// opened a conversation.
    #[test]
    fn backward_moves_not_conversations_spend_the_budget() {
        let (repo, _root_guard) = fixture("warm-loop");
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(2);

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        // Six lanes on the `work->retry` route, but only one arrival at `retry`.
        task.front.steps.insert("work->retry".into(), 6);
        task.front.arrivals.insert("retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        assert_eq!(
            queued(&repo, "stuck").stage(),
            "retry",
            "six retried prompts on the same lap must not spend a budget of two laps"
        );
    }

    /// A spent budget parks on `blocked`, which is a request for a person, so
    /// an unattended run with nobody staffing `blocked` skips the budget
    /// outright rather than spending it against a wall the run's own resume
    /// can only walk back into — it hands nothing back now. The other half,
    /// where a lane does staff `blocked` and the bound binds, is
    /// `a_budget_bound_does_bind_an_unattended_run_that_staffs_blocked`.
    #[test]
    fn an_unattended_run_skips_a_budget_whose_exit_is_a_person() {
        let (repo, _root_guard) = unattended_fixture("unattended-on-max-blocked");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(1);

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.arrivals.insert("retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "retry",
            "the loop goes round rather than parking"
        );
        assert_eq!(
            task.rounds_at("retry"),
            2,
            "the bound is skipped outright, not spent and refunded, so this move still \
             banks an arrival exactly as an unbounded one would"
        );
    }

    /// A pipeline with one gated step, and a plain one after it to be let
    /// through to. Nothing shipped gates, so a gate test builds its own.
    fn gate_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
             - id: build\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    on_pass: deploy\n  \
             - id: deploy\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    gate: true\n    loop: 2\n    on_pass: announce\n    \
               on_fail: build\n  \
             - id: announce\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    on_pass: done\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );
        pipelines
    }

    /// A task parked on the gate, as `report` would have left it.
    fn paused_at_deploy(repo: &Repo, id: &str) {
        add(repo, id, &[]);
        let mut task = queued(repo, id);
        task.set_stage("deploy", None);
        task.front.paused_at = Some("deploy".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();
    }

    /// A lane can no longer answer its own gate: `spoolway resume` run with
    /// `from_step: Some(...)` — what the CLI boundary passes when
    /// `SPOOLWAY_STEP` is set — is refused before it ever reads the task,
    /// naming why, unless that step is `blocked`.
    #[test]
    fn resume_refuses_from_inside_a_lanes_own_environment() {
        let (repo, _root_guard) = fixture("gate-jumper");
        let pipelines = gate_pipelines();
        paused_at_deploy(&repo, "ship");

        let err = resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            Some("deploy"),
        )
        .expect_err("a lane must not be able to answer its own gate");
        let said = format!("{err:#}");
        assert!(said.contains("lane's own environment"), "{said}");
        assert!(said.contains("Ask the person watching the board"), "{said}");

        // Refused before anything moved.
        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
    }

    /// The one exception to the refusal above: a lane on `blocked` may put
    /// another stopped task back on its step, since clearing a block is
    /// often a question about another task.
    #[test]
    fn a_lane_on_blocked_may_resume_another_stopped_task() {
        let (repo, _root_guard) = fixture("blocked-lane-resumes-sibling");
        let pipelines = gate_pipelines();
        add(&repo, "sibling", &[]);
        let mut task = queued(&repo, "sibling");
        task.front.blocked_from = Some("build".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "sibling".into(),
                stage: None,
                message: None,
            },
            Some(crate::pipeline::BLOCKED),
        )
        .unwrap();

        let task = queued(&repo, "sibling");
        assert_eq!(task.stage(), "build");
    }

    fn resume_args(task: &str, stage: Option<&str>) -> crate::cli::ResumeArgs {
        crate::cli::ResumeArgs {
            task: task.into(),
            stage: stage.map(str::to_string),
            message: None,
        }
    }

    /// A task whose lane is running a step is not stopped, so there is
    /// nothing to resume. Rewinding it would leave the lane's own later
    /// report refused.
    #[test]
    fn a_running_task_is_refused_by_resume_and_stays_where_it_is() {
        let (repo, _root_guard) = fixture("resume-running");
        let pipelines = gate_pipelines();
        add(&repo, "busy", &[]);
        let mut task = queued(&repo, "busy");
        task.set_stage("deploy", None);
        task.save().unwrap();

        for stage in [None, Some("build")] {
            let err = resume(&repo, &pipelines, &resume_args("busy", stage), None)
                .expect_err("a running task is not resumable");
            let said = format!("{err:#}");
            assert!(said.contains("busy") && said.contains("deploy"), "{said}");
        }
        assert_eq!(queued(&repo, "busy").stage(), "deploy");
    }

    /// A task still waiting in the queue has not started, so `--stage` may
    /// not skip it past the steps in front of the one it names.
    #[test]
    fn a_still_queued_task_is_refused_by_resume_stage_and_skips_no_steps() {
        let (repo, _root_guard) = fixture("resume-queued");
        let pipelines = gate_pipelines();
        add(&repo, "waiting", &[]);

        let err = resume(
            &repo,
            &pipelines,
            &resume_args("waiting", Some("announce")),
            None,
        )
        .expect_err("a queued task is not resumable");
        let said = format!("{err:#}");
        assert!(
            said.contains("waiting") && said.contains("queued"),
            "{said}"
        );
        assert_eq!(queued(&repo, "waiting").stage(), crate::pipeline::QUEUED);
    }

    /// A child may not be resumed onto a step while a task it depends on has
    /// not finished. The refusal names that task and its state.
    #[test]
    fn resume_stage_is_refused_while_a_task_it_depends_on_is_not_done() {
        let (repo, _root_guard) = fixture("resume-unmet-dependency");
        let pipelines = gate_pipelines();
        add(&repo, "parent", &[]);
        let mut parent = queued(&repo, "parent");
        parent.front.blocked_from = Some("build".into());
        parent.set_stage(crate::pipeline::BLOCKED, None);
        parent.save().unwrap();
        add(&repo, "child", &["parent"]);
        let mut child = queued(&repo, "child");
        child.front.blocked_from = Some("build".into());
        child.set_stage(crate::pipeline::BLOCKED, None);
        child.save().unwrap();

        let err = resume(
            &repo,
            &pipelines,
            &resume_args("child", Some("deploy")),
            None,
        )
        .expect_err("a child must wait for its parent");
        let said = format!("{err:#}");
        assert!(
            said.contains("parent") && said.contains(crate::pipeline::BLOCKED),
            "{said}"
        );
        assert_eq!(queued(&repo, "child").stage(), crate::pipeline::BLOCKED);
    }

    /// A task on a stage no pipeline defines is not running anything: a
    /// pipeline edited under it must still be reachable by hand.
    #[test]
    fn a_task_on_an_unknown_stage_stays_resumable() {
        let pipelines = looping_pipelines(2);
        let (id, stage) = ("gone", "removed-step");
        let (repo, _root_guard) = fixture(&format!("resume-unknown-stage-{id}"));
        add(&repo, id, &[]);
        let mut task = queued(&repo, id);
        task.set_stage_unbanked(stage, "test setup");
        task.save().unwrap();

        resume(&repo, &pipelines, &resume_args(id, Some("work")), None)
            .unwrap_or_else(|e| panic!("`{id}` on `{stage}` must resume: {e:#}"));
        assert_eq!(queued(&repo, id).stage(), "work");
    }

    /// A dependency that is neither queued nor archived is unknown, which
    /// the dispatcher holds a task for, so `--stage` may not go past it.
    #[test]
    fn resume_stage_is_refused_for_a_dependency_that_is_not_a_task() {
        let (repo, _root_guard) = fixture("resume-unknown-dependency");
        let pipelines = gate_pipelines();
        add(&repo, "lgoin", &[]);
        add(&repo, "child", &["lgoin"]);
        // Queueing refuses an unknown dependency, so the parent leaves the
        // queue afterwards without being archived.
        std::fs::remove_file(repo.queue_dir().join("lgoin.md")).unwrap();
        let mut child = queued(&repo, "child");
        child.front.blocked_from = Some("build".into());
        child.set_stage(crate::pipeline::BLOCKED, None);
        child.save().unwrap();

        let err = resume(
            &repo,
            &pipelines,
            &resume_args("child", Some("deploy")),
            None,
        )
        .expect_err("an unknown dependency holds the child");
        let said = format!("{err:#}");
        assert!(
            said.contains("lgoin") && said.contains("not a task"),
            "{said}"
        );
    }

    /// A parent that is running rather than stopped cannot be resumed, so the
    /// refusal tells the person to wait instead of to resume it.
    #[test]
    fn resume_stage_tells_a_person_to_wait_for_a_running_parent() {
        let (repo, _root_guard) = fixture("resume-running-parent");
        let pipelines = gate_pipelines();
        add(&repo, "parent", &[]);
        let mut parent = queued(&repo, "parent");
        parent.set_stage("build", None);
        parent.save().unwrap();
        add(&repo, "child", &["parent"]);
        let mut child = queued(&repo, "child");
        child.front.blocked_from = Some("build".into());
        child.set_stage(crate::pipeline::BLOCKED, None);
        child.save().unwrap();

        let err = resume(
            &repo,
            &pipelines,
            &resume_args("child", Some("deploy")),
            None,
        )
        .expect_err("a running parent holds the child");
        let said = format!("{err:#}");
        assert!(said.contains("wait for parent"), "{said}");
        assert!(!said.contains("resume parent"), "{said}");
    }

    /// A parent on `done` is still in the queue until cleanup archives it, and
    /// it holds the child. The refusal says why, rather than calling a `done`
    /// task unfinished without explanation.
    #[test]
    fn resume_stage_explains_a_parent_that_is_done_but_not_archived() {
        let (repo, _root_guard) = fixture("resume-done-parent");
        let pipelines = gate_pipelines();
        add(&repo, "parent", &[]);
        add(&repo, "child", &["parent"]);
        let mut parent = queued(&repo, "parent");
        parent.set_stage(crate::pipeline::DONE, None);
        parent.save().unwrap();
        let mut child = queued(&repo, "child");
        child.front.blocked_from = Some("build".into());
        child.set_stage(crate::pipeline::BLOCKED, None);
        child.save().unwrap();

        let err = resume(
            &repo,
            &pipelines,
            &resume_args("child", Some("deploy")),
            None,
        )
        .expect_err("a parent still in the queue holds the child");
        let said = format!("{err:#}");
        assert!(
            said.contains("done but not yet cleaned up and archived"),
            "{said}"
        );
        assert!(said.contains("wait for parent"), "{said}");
    }

    /// A lane on `blocked` may unblock other stopped tasks but never the task
    /// it is itself working on.
    #[test]
    fn a_lane_on_blocked_may_not_resume_its_own_task() {
        let (repo, _root_guard) = fixture("blocked-lane-resumes-itself");
        let pipelines = gate_pipelines();
        add(&repo, "mine", &[]);
        let mut task = queued(&repo, "mine");
        task.front.blocked_from = Some("build".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let err = crate::platform::test_env::with_env(TASK_ENV, "mine", || {
            resume(
                &repo,
                &pipelines,
                &resume_args("mine", None),
                Some(crate::pipeline::BLOCKED),
            )
        })
        .expect_err("a lane may not unblock its own task");
        let said = format!("{err:#}");
        assert!(said.contains("mine") && said.contains("own"), "{said}");
        assert_eq!(queued(&repo, "mine").stage(), crate::pipeline::BLOCKED);
    }

    /// Bounded even from `blocked`: a task waiting on a gate is a person's to
    /// answer, and `--stage` is not a lane's to hand it either.
    #[test]
    fn a_lane_on_blocked_may_not_resume_past_a_gate_or_reroute() {
        let (repo, _root_guard) = fixture("blocked-lane-cannot-cross-a-gate");
        let pipelines = gate_pipelines();
        paused_at_deploy(&repo, "ship");

        let gate = resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            Some(crate::pipeline::BLOCKED),
        )
        .expect_err("a lane must not answer a gate on another task's behalf");
        assert!(format!("{gate:#}").contains("waiting on a gate"));

        // Depends on `ship`: both share `add`'s own `group: demo`, and a
        // group is one chain now — this test has no stake in the two being
        // unrelated, only in `sibling` itself being a second, distinct task.
        add(&repo, "sibling", &["ship"]);
        let mut task = queued(&repo, "sibling");
        task.front.blocked_from = Some("build".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let rerouted = resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "sibling".into(),
                stage: Some("announce".into()),
                message: None,
            },
            Some(crate::pipeline::BLOCKED),
        )
        .expect_err("a lane must not reroute another task by hand");
        assert!(format!("{rerouted:#}").contains("--stage"));
    }

    /// The whole of what a gate is: spoolway does not act on a gated step's
    /// pass, and a person's `resume` is the thing that does.
    ///
    /// The old arrangement asked the *lane* to stop and ask, which is a request
    /// rather than a mechanism — three plan runs against a small local model,
    /// three gated steps, and every one of them reported a pass and walked
    /// straight through.
    #[test]
    fn a_gated_steps_pass_waits_for_a_person_and_resume_lets_it_past() {
        let (repo, _root_guard) = fixture("gate-release");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("deployed".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("deploy"));
        assert_eq!(task.front.paused_by.as_deref(), Some("gate"));

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: Some("looks right".into()),
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), "announce", "resume takes the `on_pass` route");
        assert_eq!(task.front.paused_at, None);
        assert_eq!(task.front.paused_by, None, "cleared alongside paused_at");
    }

    /// A gated pass writes two status-log lines, not one: spoolway's own
    /// arrival note, credited to no step, and the lane's own `-m` right
    /// under it, credited to the step it reported from — the Mockup's own
    /// shape. `set_stage`'s arrival line used to be the only one, and the
    /// lane's account of its own pass was thrown away entirely.
    #[test]
    fn a_gated_pass_writes_the_arrival_note_and_the_lanes_own_message() {
        let (repo, _root_guard) = fixture("gate-hands-over-status-log");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("Rendering matches the mockup exactly.".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        let log = task.section("## Status Log").unwrap_or_default();
        let arrival = log
            .lines()
            .find(|line| line.contains("held by this step's own gate"))
            .unwrap_or_else(|| panic!("no gate arrival line: {log}"));
        assert!(
            arrival.contains("→ `paused`"),
            "the arrival line is spoolway's own: {log}"
        );
        let own_message = log
            .lines()
            .find(|line| line.contains("Rendering matches the mockup exactly."))
            .unwrap_or_else(|| panic!("the lane's own message never reached the log: {log}"));
        assert!(
            own_message.contains("`deploy`:") && !own_message.contains('→'),
            "credited to the step it reported from, with no arrow — it is not a transition: {log}"
        );
    }

    /// `caught_at` — read by `resume` and the board alike — must still tell a
    /// gate catch from a pause raised at `blocked` itself for a task that
    /// paused before `paused_by` existed: `report` strips it here rather than
    /// setting it, the same shape a task file written before this key would
    /// carry. The fallback is `last_report.step == gated`, exactly what
    /// `caught_at` always inferred this from.
    #[test]
    fn a_gated_pass_with_no_paused_by_still_resumes_by_the_old_inference() {
        let (repo, _root_guard) = fixture("gate-release-no-paused-by");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("deployed".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();

        let mut task = queued(&repo, "ship");
        assert_eq!(task.front.paused_by.as_deref(), Some("gate"));
        task.front.paused_by = None;
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(
            task.stage(),
            "announce",
            "resume still takes the on_pass route"
        );
    }

    /// `paused_by` is what actually decides `caught_at` now, not a second
    /// check against `last_report.step` that happens to agree with it on
    /// every real report — a task whose `last_report` names a step other
    /// than the one it paused at (never written by `report` itself, but
    /// nothing stops a hand edit) still reads as a catch once `paused_by` is
    /// set, which the old `last_report.step == gated` check alone would
    /// have refused.
    #[test]
    fn paused_by_alone_is_enough_for_caught_at_to_read_a_catch() {
        let (repo, _root_guard) = fixture("caught-at-paused-by-alone");
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.paused_at = Some("deploy".into());
        task.front.paused_by = Some("schedule".into());
        task.front.last_report = Some(crate::task::LastReport {
            step: "build".into(),
            outcome: "pass".into(),
            at: 0,
            blocked: false,
        });
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        assert_eq!(
            caught_at(&task, "deploy"),
            Some(Caught::Pass),
            "paused_by alone should be enough, whatever last_report.step names"
        );
    }

    /// The other road to the same pause: a task's own `gate_at`, set by
    /// whoever wrote its task, holds it after a step the pipeline never
    /// declared `gate: true` on — `build` here, which routes straight to
    /// `deploy` for every other task on this pipeline.
    #[test]
    fn a_tasks_own_gate_at_pauses_a_step_the_pipeline_never_gated() {
        let (repo, _root_guard) = fixture("gate-at");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.gate_at = Some("build".into());
        task.set_stage("build", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("built".into()),
                handoff: vec![],
            },
            Some("build"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("build"));
        assert_eq!(task.front.paused_by.as_deref(), Some("schedule"));
        // Spent the moment it fired, unlike a pipeline's own `gate: true`,
        // which never comes off — a later route back onto `build` must not
        // find it still armed.
        assert_eq!(task.front.gate_at, None);

        // `resume` reads `paused_at` exactly as it does for a pipeline's own
        // gate — nothing downstream needed to learn a second way to get here.
        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), "deploy", "resume takes `build`'s on_pass");

        // A later route back onto `build` — `deploy` failing round-trips
        // here through its own `on_fail` — passes straight through this
        // time: the schedule was a one-time answer, not a standing gate.
        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: false,
                fail: true,
                block: false,
                pause: false,
                message: Some("needs another pass".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();
        assert_eq!(queued(&repo, "ship").stage(), "build");

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("built again".into()),
                handoff: vec![],
            },
            Some("build"),
        )
        .unwrap();
        let task = queued(&repo, "ship");
        assert_eq!(
            task.stage(),
            "deploy",
            "the second arrival at `build` is not gated again"
        );
        assert_eq!(task.front.paused_at, None);
    }

    /// A schedule is not choosy about what it catches — a fail holds the task
    /// on `paused` exactly as a pass would, rather than being let round
    /// `deploy`'s own `on_fail` loop back to `build`. `spoolway resume` then
    /// sends a caught fail on by `deploy`'s own `on_pass`: taking that
    /// verdict is why the pause was scheduled in the first place. `deploy`
    /// rather than `build`, whose own `on_fail` is the default `blocked` — a
    /// caught fail bound for `blocked` anyway is
    /// `a_tasks_own_gate_at_catches_a_block_and_resume_sends_it_to_blocked`'s
    /// own case, not this one.
    #[test]
    fn a_tasks_own_gate_at_catches_a_failing_step_and_resume_takes_on_pass() {
        let (repo, _root_guard) = fixture("gate-at-fail");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.gate_at = Some("deploy".into());
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: false,
                fail: true,
                block: false,
                pause: false,
                message: Some("the migration failed".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(
            task.stage(),
            crate::pipeline::PAUSED,
            "a schedule catches a fail, not only a pass"
        );
        assert_eq!(task.front.paused_at.as_deref(), Some("deploy"));
        assert_eq!(task.front.gate_at, None, "spent the moment it fired");
        assert_eq!(
            task.front.blocked_from, None,
            "`deploy`'s own on_fail is not `blocked`, so nothing here should have set it"
        );

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "ship").stage(),
            "announce",
            "a caught fail still goes on by `on_pass`, not round `deploy`'s own on_fail loop"
        );
    }

    /// A schedule catches a block too, and everything a raw `--block` already
    /// carries survives the catch: `set_blocked_from` still runs ahead of it,
    /// so `blocked_from` names the step it stopped on. `spoolway resume` reads
    /// that back and sends a caught block straight to `blocked` — exactly
    /// where it would have landed unheld.
    #[test]
    fn a_tasks_own_gate_at_catches_a_block_and_resume_sends_it_to_blocked() {
        let (repo, _root_guard) = fixture("gate-at-block");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.gate_at = Some("build".into());
        task.set_stage("build", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: false,
                fail: false,
                block: true,
                pause: false,
                message: Some("needs a credential no lane can mint".into()),
                handoff: vec![],
            },
            Some("build"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("build"));
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("build"),
            "set_blocked_from still runs ahead of the catch"
        );
        assert_eq!(task.front.gate_at, None);

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "ship").stage(),
            crate::pipeline::BLOCKED,
            "a caught block goes to `blocked`, exactly where it would have landed unheld"
        );
    }

    /// The bug review finding 4 caught: `caught_at` read a *stale*
    /// `blocked_from` as a caught block, because nothing cleared it once the
    /// task moved past the step it named without landing on `blocked`. Spend
    /// exactly the road that used to leave it standing — a caught block,
    /// rerouted with `--stage` to a step other than `blocked` — then send
    /// the same step round again with a fresh schedule and a clean pass, and
    /// the pass must still read as a pass.
    #[test]
    fn a_caught_pass_is_not_read_as_a_block_because_an_old_blocked_from_still_names_the_step() {
        let (repo, _root_guard) = fixture("gate-at-stale-blocked-from");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.gate_at = Some("deploy".into());
        task.set_stage("deploy", None);
        task.save().unwrap();

        // A caught block, rerouted with `--stage` onto `build` — which is
        // what used to leave `blocked_from: deploy` standing.
        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: false,
                fail: false,
                block: true,
                pause: false,
                message: Some("the forge is down".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();
        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: Some("build".into()),
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(queued(&repo, "ship").stage(), "build");
        assert_eq!(queued(&repo, "ship").front.blocked_from, None);

        // Back to `deploy`, freshly scheduled, and this time it passes clean.
        let mut task = queued(&repo, "ship");
        task.front.gate_at = Some("deploy".into());
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("deployed".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();
        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(
            task.front.blocked_from, None,
            "nothing blocked this time, so nothing should have set it"
        );

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "ship").stage(),
            "announce",
            "a clean pass must go on by `on_pass`, not to `blocked` off a stale `blocked_from`"
        );
    }

    /// A destination `apply_loop_budget` turns into `blocked` is caught the
    /// same way as a raw `--block`: the schedule holds the failure that spent
    /// the budget on `paused` rather than letting it land on `blocked` at
    /// once, and a plain `resume` sends it on to `blocked` all the same.
    #[test]
    fn a_tasks_own_gate_at_catches_a_spent_loop_headed_for_blocked() {
        let (repo, _root_guard) = fixture("gate-at-loop-max");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // The shipped `default` pipeline, minus its `blocked` step — the same
        // shape
        // `a_budget_bound_for_a_person_does_not_bind_an_unattended_run_with_no_staffed_blocked_step`
        // builds, for an attended run instead, where the bound binds whether
        // or not a lane staffs `blocked`.
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
        }

        let spent = implement_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.front.gate_at = Some("review".into());
        task.set_stage("review", None);
        task.front.arrivals.insert("implement".into(), spent);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            crate::pipeline::PAUSED,
            "the schedule catches the spent loop's own `blocked` exit"
        );
        assert_eq!(task.front.paused_at.as_deref(), Some("review"));
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("review"),
            "set_blocked_from ran for the loop's own `blocked` exit before the catch"
        );
        assert_eq!(task.front.gate_at, None);

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(
            queued(&repo, "stuck").stage(),
            crate::pipeline::BLOCKED,
            "a caught loop-max goes to `blocked` too, exactly where it would have landed unheld"
        );
    }

    /// `gate:` is not one of the checks unattended skips. A run with nobody
    /// in it still has no one to approve a gate, so the pass parks on
    /// `paused` exactly as it would in an attended run, and waits for
    /// `spoolway resume` regardless of who is watching.
    #[test]
    fn an_unattended_gated_pass_still_parks_on_paused() {
        let (repo, _root_guard) = unattended_fixture("gate-unattended");
        let pipelines = gate_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("ship".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("deployed".into()),
                handoff: vec![],
            },
            Some("deploy"),
        )
        .unwrap();

        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("deploy"));
    }

    /// `resume` reads which of the two questions a paused task is asking, so
    /// the person naming it never has to: with no override it goes past the
    /// gate, the same as `release` used to.
    #[test]
    fn resuming_a_paused_task_with_no_stage_lets_it_past_the_gate() {
        let (repo, _root_guard) = fixture("gate-unblock");
        let pipelines = gate_pipelines();
        paused_at_deploy(&repo, "ship");

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), "announce", "`deploy`'s on_pass route");
        assert_eq!(task.front.paused_at, None);
    }

    /// Naming a step by hand is still a person overriding the route, even on
    /// a paused task — `--stage` reroutes it instead of letting it past the
    /// gate.
    #[test]
    fn resuming_a_paused_task_with_a_stage_reroutes_it_instead() {
        let (repo, _root_guard) = fixture("gate-unblock");
        let pipelines = gate_pipelines();
        paused_at_deploy(&repo, "ship");

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "ship".into(),
                stage: Some("build".into()),
                message: None,
            },
            None,
        )
        .unwrap();
        let task = queued(&repo, "ship");
        assert_eq!(task.stage(), "build");
        assert_eq!(task.front.paused_at, None);
    }

    /// A person's resume of a task held on `blocked` starts every count
    /// again, like the unblocker's pass: the step it goes back to is not
    /// walked straight into the limit that sent the task there.
    #[test]
    fn a_hand_resume_from_blocked_resets_every_arrival_count() {
        let (repo, _root_guard) = fixture("unblock-keeps-arrivals-spent");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let pipelines = Pipelines::builtin();

        // Spent right up to `implement`'s own limit, and one arrival at the
        // other end of the pipeline that nobody has looked at.
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front
            .arrivals
            .insert("implement".into(), implement_limit(&pipelines));
        task.front.arrivals.insert("refresh".into(), 1);
        task.front.blocked_from = Some("review".into());
        task.set_stage("blocked", None);
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review");
        assert_eq!(
            task.rounds_at("implement"),
            0,
            "the spent budget starts again"
        );
        assert_eq!(
            task.rounds_at("refresh"),
            0,
            "and so does every other step's"
        );
        // Only the one lap this move itself banked: the history before it is gone.
        assert_eq!(task.front.rounds.len(), 1, "{:?}", task.front.rounds);

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        assert_eq!(
            queued(&repo, "stuck").stage(),
            "implement",
            "the next failure is the first arrival at `implement` again, not a refusal"
        );
    }

    /// The board's STEP column reads `Task::rounds_at`, which answers off
    /// `Frontmatter::arrivals` — a map of its own, banked beside `rounds` in
    /// `Task::set_stage`. `resume_at` refunds nothing, so a resume from a step
    /// that is not `blocked` keeps what a task has genuinely stood at: here a
    /// task held on `paused` after standing at `implement` twice keeps showing
    /// `↻2` when a person resumes it.
    #[test]
    fn a_hand_resume_not_from_blocked_does_not_erase_a_steps_arrival_count() {
        let (repo, _root_guard) = fixture("unblock-keeps-arrivals");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None); // queued->implement: 1
        task.set_stage("review", None); // implement->review: 1
        task.set_stage("implement", None); // review->implement: 1
        task.set_stage("review", None); // implement->review: 2
        task.save().unwrap();

        let before = queued(&repo, "stuck");
        assert_eq!(
            before.rounds_at("implement"),
            2,
            "two genuine arrivals at `implement` before anything is resumed"
        );

        let mut task = before;
        task.front.blocked_from = Some("review".into());
        task.set_stage("paused", None);
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review");
        assert_eq!(
            task.rounds_at("implement"),
            2,
            "a hand resume touches neither `rounds` nor `arrivals` — `implement` was \
             still visited twice"
        );
    }

    /// The lane that blocked did the reading, the exploring and usually the
    /// work; a fresh session on the same step pays for all of it again to get
    /// back where that one already was. Resuming marks the step to be
    /// continued instead, and names the step so that a launch of any *other*
    /// one cannot pick the mark up by accident.
    #[test]
    fn resuming_marks_the_step_it_resumes_to_be_continued() {
        let (repo, _root_guard) = fixture("unblock-marks-resume");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.blocked_from = Some("review".into());
        task.set_stage("blocked", None);
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        assert_eq!(
            queued(&repo, "stuck").front.resume.as_deref(),
            Some("review")
        );
    }

    /// A task parked before it ever started — `p` on a `queued` row, which
    /// records no `parked_from` — goes back onto `queued`, not the entry
    /// step: it is the `queued` arm that applies the dependency and hook
    /// gates, and a resume that lands past it launches the task off a
    /// dependency still mid-work (jobs review finding 1).
    ///
    /// The round trip banks nothing, the same as `unpark`'s own: the task
    /// never left `queued`, so `paused->queued` is a lap no pipeline routed,
    /// not one to count against a `loop:` budget.
    #[test]
    fn resuming_a_task_parked_off_queued_lands_it_back_on_queued() {
        let (repo, _root_guard) = fixture("unpark-from-queued");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        crate::status::park(&mut task, "paused from the board", false);
        task.save().unwrap();
        assert_eq!(
            task.front.parked_from, None,
            "what `park` leaves on `queued`"
        );
        let arrived_from_before = task.front.arrived_from.clone();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), crate::pipeline::QUEUED);
        assert_eq!(task.front.paused_at, None);
        assert!(task.front.rounds.is_empty(), "{:?}", task.front.rounds);
        assert_eq!(task.front.arrived_from, arrived_from_before);
    }

    /// A bare `spoolway resume` on a `p`-parked task reaches `unpark`, not
    /// `resume_at`: the round trip banks no lap, gives nothing back (there
    /// was nothing to give back), and marks the step to be continued the
    /// same way a real resume does.
    #[test]
    fn resuming_a_parked_task_puts_it_back_without_banking_a_lap() {
        let (repo, _root_guard) = fixture("unpark-round-trip");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        let rounds_before = task.front.rounds.clone();
        let arrivals_before = task.front.arrivals.clone();
        let prompts_before = task.front.steps.clone();
        let arrived_from_before = task.front.arrived_from.clone();
        task.front.parked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.rounds, rounds_before);
        assert_eq!(task.front.arrivals, arrivals_before);
        assert_eq!(task.front.steps, prompts_before);
        assert_eq!(task.front.arrived_from, arrived_from_before);
        assert_eq!(task.front.blocked_from, None);
        assert_eq!(task.front.resume.as_deref(), Some("implement"));
        let log = task.section("## Status Log").unwrap();
        assert!(log.contains("put back from the board"), "{log}");
        assert!(!log.to_lowercase().contains("unblocked"), "{log}");
    }

    /// A `--stage` reroute past a park leaves `back_onto_its_step`'s ordinary
    /// road rather than `unpark`'s, but the park is answered all the same —
    /// `parked_from` must not survive it, or a later, ordinary retry of the
    /// step it once named would read at launch as a park nobody asked for.
    #[test]
    fn rerouting_a_parked_task_with_a_stage_clears_parked_from_too() {
        let (repo, _root_guard) = fixture("unpark-stage-reroute");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.front.parked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: Some("review".into()),
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review");
        assert_eq!(
            task.front.parked_from, None,
            "a reroute past a park has answered it just as much as putting it \
             back would — left set, `finish_launch_bookkeeping` would read the step's \
             next ordinary retry as a continued park"
        );
    }

    /// The same race a real resume closes for its own lane, closed for a
    /// park too: a stale settled lane left on the step a park just put the
    /// task back on would otherwise read as a lane still waiting for an
    /// answer, and no fresh one would ever start.
    #[test]
    fn unparking_frees_the_stale_lane_it_left_behind() {
        let (mut repo, _root_guard) = fixture("unpark-stale-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.front.parked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        let headless = crate::headless::Headless::new(&repo.root, &repo.home, repo.headless_dir());
        headless
            .start_lane(
                &crate::mux::LaneSpec {
                    name: &crate::mux::lane_name("implement", "stuck"),
                    label: "implementer",
                    kind: "pi",
                    pane_id: "p1",
                    args: &[],
                    env: &std::collections::BTreeMap::new(),
                    path_prefix: None,
                },
                &mut || {},
            )
            .unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        assert_eq!(queued(&repo, "stuck").stage(), "implement");
        assert!(
            headless.list_lanes().unwrap().is_empty(),
            "the stale lane must be freed, or the next pass waits on it forever"
        );
    }

    /// A resume closes the settled lane on the step it sends the task to, and
    /// no other. The task's lanes on steps it has already run are kept open
    /// until it is done, so a person can read what they said; closing them
    /// with every resume would throw that away.
    #[test]
    fn resuming_closes_only_the_lane_on_the_step_it_resumes() {
        let (mut repo, _root_guard) = fixture("unpark-only-resumed-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.front.parked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        let headless = crate::headless::Headless::new(&repo.root, &repo.home, repo.headless_dir());
        for step in ["implement", "review"] {
            headless
                .start_lane(
                    &crate::mux::LaneSpec {
                        name: &crate::mux::lane_name(step, "stuck"),
                        label: step,
                        kind: "pi",
                        pane_id: "p1",
                        args: &[],
                        env: &std::collections::BTreeMap::new(),
                        path_prefix: None,
                    },
                    &mut || {},
                )
                .unwrap();
        }

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let left: Vec<String> = headless
            .list_lanes()
            .unwrap()
            .into_iter()
            .map(|lane| lane.name)
            .collect();
        assert_eq!(
            left,
            [crate::mux::lane_name("review", "stuck")],
            "the lane on the resumed step is closed, the one on another step stays"
        );
    }

    /// A person who names a stage has rerouted the task rather than resumed it.
    /// The step they chose may have run hours and several steps ago, and
    /// reopening that conversation to answer a decision it never heard is not
    /// what they asked for — that one starts fresh.
    #[test]
    fn resuming_to_a_named_stage_starts_that_step_fresh() {
        let (repo, _root_guard) = fixture("unblock-elsewhere");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.blocked_from = Some("review".into());
        task.set_stage("blocked", None);
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: Some("implement".into()),
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "implement");
        assert_eq!(
            task.front.resume, None,
            "a reroute is not a continuation: nothing of the old lane's is \
             carried into a step the person picked"
        );
    }

    /// What the shipped `implement` step allows itself to arrive at, read
    /// rather than written down twice.
    fn implement_limit(pipelines: &Pipelines) -> u32 {
        pipelines
            .get("default")
            .unwrap()
            .step("implement")
            .unwrap()
            .arrival_limit()
            .unwrap()
    }

    /// The race `resume` has to close itself: the lane that reported the
    /// block is still sitting settled in its pane until a dispatch pass frees
    /// it — and once the task is back on that lane's step, a pass reads the
    /// stale lane as "waiting for an answer" and never starts a fresh one. A
    /// person who resumes between two passes (ten minutes apart by default)
    /// would wedge the task forever.
    #[test]
    fn resuming_frees_the_stale_lane_that_blocked() {
        let (mut repo, _root_guard) = fixture("unblock-stale-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = Some("handover".into());
        task.set_stage("blocked", None);
        task.save().unwrap();

        // The lane that blocked, exactly as it survives a report: settled in
        // its pane, never yet freed by a pass.
        let headless = crate::headless::Headless::new(&repo.root, &repo.home, repo.headless_dir());
        headless
            .start_lane(
                &crate::mux::LaneSpec {
                    name: "stuck · handover",
                    label: "github",
                    kind: "pi",
                    pane_id: "p1",
                    args: &[],
                    env: &std::collections::BTreeMap::new(),
                    path_prefix: None,
                },
                &mut || {},
            )
            .unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "stuck".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        assert_eq!(queued(&repo, "stuck").stage(), "handover");
        assert!(
            headless.list_lanes().unwrap().is_empty(),
            "the stale lane must be freed, or the next pass waits on it forever"
        );
    }

    /// A pane-keeping backend (`resident_while_waiting()` is true) with one
    /// scripted lane, recording what `restart` does to it. `on_list` runs when
    /// lanes are listed, standing in for a report that lands before the
    /// restart is written; `on_stop` runs when a lane is stopped.
    struct RestartMux {
        lanes: Vec<crate::mux::Lane>,
        calls: std::sync::Mutex<Vec<String>>,
        on_list: Box<dyn Fn() + Send + Sync>,
        on_stop: Box<dyn Fn() + Send + Sync>,
    }

    impl RestartMux {
        fn with_lane(repo: &Repo, task: &str, status: crate::mux::LaneStatus) -> Self {
            Self {
                lanes: vec![crate::mux::Lane {
                    name: crate::mux::lane_name("implement", task),
                    kind: "pi".into(),
                    status,
                    pane_id: "p1".into(),
                    tab_id: "t1".into(),
                    workspace_id: "w1".into(),
                    cwd: repo.root.clone(),
                    launch_pending: None,
                    interactive_ready: None,
                }],
                calls: std::sync::Mutex::new(Vec::new()),
                on_list: Box::new(|| {}),
                on_stop: Box::new(|| {}),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl crate::mux::Mux for RestartMux {
        fn name(&self) -> &'static str {
            "restart-fake"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            String::new()
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<crate::mux::Lane>> {
            (self.on_list)();
            Ok(self.lanes.clone())
        }
        fn create_workspace(
            &self,
            _cwd: &Path,
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
        fn create_pane(&self, _cwd: &Path, _label: &str) -> Result<crate::mux::Workspace> {
            unimplemented!()
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &Path) -> Result<String> {
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
        fn interrupt_lane(&self, name: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("interrupt {name}"));
            Ok(())
        }
        fn stop_lane(&self, name: &str, _pane_id: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("stop {name}"));
            (self.on_stop)();
            Ok(())
        }
        fn focus_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
    }

    fn restart_args(task: &str) -> crate::cli::RestartArgs {
        crate::cli::RestartArgs {
            task: task.into(),
            message: None,
        }
    }

    /// The teardown the dispatcher's escalation skips on a pane-keeping
    /// backend: a working lane is interrupted and then stopped anyway, because
    /// the pane holds the conversation being discarded. The task lands on the
    /// step it was already on, asking for a fresh session.
    #[test]
    fn restarting_stops_a_working_lane_even_on_a_pane_keeping_backend() {
        let (repo, _root_guard) = fixture("restart-working");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.front.attempts = 2;
        task.front.resume = Some("implement".into());
        task.save().unwrap();
        let mux = RestartMux::with_lane(&repo, "stuck", crate::mux::LaneStatus::Working);
        assert!(crate::mux::Mux::resident_while_waiting(&mux));

        restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux).unwrap();

        let lane = crate::mux::lane_name("implement", "stuck");
        assert_eq!(
            mux.calls(),
            [format!("interrupt {lane}"), format!("stop {lane}")]
        );
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.restart.as_deref(), Some("implement"));
        assert_eq!(task.front.resume, None, "a restart continues nothing");
        assert_eq!(task.front.attempts, 0);
        assert!(
            task.section("## Status Log")
                .unwrap()
                .contains("→ `implement`: restarted by hand")
        );
    }

    /// A settled lane has no turn to interrupt, but is still stopped.
    #[test]
    fn restarting_stops_a_settled_lane_without_interrupting_it() {
        let (repo, _root_guard) = fixture("restart-settled");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.save().unwrap();
        let mux = RestartMux::with_lane(&repo, "stuck", crate::mux::LaneStatus::Done);

        restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux).unwrap();

        assert_eq!(
            mux.calls(),
            [format!(
                "stop {}",
                crate::mux::lane_name("implement", "stuck")
            )]
        );
    }

    /// A restart ends the lane on the step it starts over and no other. The
    /// lane on a step the task already ran keeps its pane until the task is
    /// done, as it does after a resume.
    #[test]
    fn restarting_keeps_the_lane_of_a_step_already_run() {
        let (repo, _root_guard) = fixture("restart-keeps-finished");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.save().unwrap();
        let mut mux = RestartMux::with_lane(&repo, "stuck", crate::mux::LaneStatus::Done);
        let mut review = mux.lanes[0].clone();
        review.name = crate::mux::lane_name("review", "stuck");
        review.status = crate::mux::LaneStatus::Working;
        mux.lanes.push(review);

        restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux).unwrap();

        let lane = crate::mux::lane_name("review", "stuck");
        assert_eq!(
            mux.calls(),
            [format!("interrupt {lane}"), format!("stop {lane}")]
        );
    }

    /// A blocked task is restarted on the step that blocked, with its loop
    /// counts reset and the block's marks cleared; `-m` is what the status
    /// log records.
    #[test]
    fn restarting_a_blocked_task_resets_its_loop_counts() {
        let (repo, _root_guard) = fixture("restart-blocked");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.blocked_from = Some("review".into());
        task.set_stage("blocked", None);
        task.save().unwrap();
        assert!(!task.front.rounds.is_empty());
        let mux = RestartMux {
            lanes: Vec::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            on_list: Box::new(|| {}),
            on_stop: Box::new(|| {}),
        };

        let args = crate::cli::RestartArgs {
            task: "stuck".into(),
            message: Some("the conversation looped".into()),
        };
        restart_with(&repo, &Pipelines::builtin(), &args, &mux).unwrap();

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review");
        assert_eq!(task.front.restart.as_deref(), Some("review"));
        assert_eq!(task.front.blocked_from, None);
        assert!(task.front.rounds.is_empty() && task.front.arrivals.is_empty());
        assert!(
            task.section("## Status Log")
                .unwrap()
                .contains("→ `review`: the conversation looped")
        );
    }

    /// A task that is not on a step has nothing to start over, and the refusal
    /// says what to do instead; nothing is written.
    #[test]
    fn restarting_refuses_a_queued_or_finished_task() {
        let (repo, _root_guard) = fixture("restart-refuses");
        add(&repo, "waiting", &[]);
        add(&repo, "finished", &["waiting"]);
        let mut finished = queued(&repo, "finished");
        finished.set_stage("done", None);
        finished.save().unwrap();
        let mux = RestartMux {
            lanes: Vec::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            on_list: Box::new(|| {}),
            on_stop: Box::new(|| {}),
        };

        let err = restart_with(&repo, &Pipelines::builtin(), &restart_args("waiting"), &mux)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("queued") && err.contains("queue pause waiting"),
            "{err}"
        );
        let err = restart_with(
            &repo,
            &Pipelines::builtin(),
            &restart_args("finished"),
            &mux,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("is done, which has finished") && err.contains("--stage"),
            "{err}"
        );
        assert_eq!(queued(&repo, "waiting").front.restart, None);
        assert!(mux.calls().is_empty());
    }

    /// A report that lands before the restart is written is the newer fact. The
    /// restart refuses rather than overwrite it, touches no lane, and does not
    /// claim success.
    #[test]
    fn restarting_refuses_when_a_report_lands_first() {
        let (repo, _root_guard) = fixture("restart-dropped");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.save().unwrap();
        let path = task.path.clone();
        let mut mux = RestartMux::with_lane(&repo, "stuck", crate::mux::LaneStatus::Working);
        mux.on_list = Box::new(move || {
            let mut reported = Task::load(&path).unwrap();
            reported.set_stage("review", Some("reported"));
            reported.save().unwrap();
        });

        let err = restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux)
            .unwrap_err()
            .to_string();

        assert!(err.contains("a report landed first"), "{err}");
        assert!(
            mux.calls().is_empty(),
            "no lane is touched: {:?}",
            mux.calls()
        );
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review", "the report stands");
        assert_eq!(task.front.restart, None);
    }

    /// The restart is on disk before any lane is touched, so a dispatcher pass
    /// that finds the lane gone reads `restart:` instead of relaunching the
    /// abandoned conversation.
    #[test]
    fn restarting_writes_the_task_before_it_stops_a_lane() {
        let (repo, _root_guard) = fixture("restart-order");
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.save().unwrap();
        let path = task.path.clone();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = seen.clone();
        let mut mux = RestartMux::with_lane(&repo, "stuck", crate::mux::LaneStatus::Working);
        mux.on_stop = Box::new(move || {
            *record.lock().unwrap() = Some(Task::load(&path).unwrap().front.restart);
        });

        restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux).unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            Some(Some("implement".to_string())),
            "`restart:` was already written when the lane was stopped"
        );
    }

    /// A park and a gate pause each land back on the step they hold, with the
    /// marks that described the stop cleared and `restart:` set.
    #[test]
    fn restarting_a_paused_task_lands_on_the_step_it_holds() {
        let (repo, _root_guard) = fixture("restart-paused");
        add(&repo, "parked", &[]);
        add(&repo, "gated", &["parked"]);
        let mux = RestartMux {
            lanes: Vec::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            on_list: Box::new(|| {}),
            on_stop: Box::new(|| {}),
        };

        let mut parked = queued(&repo, "parked");
        parked.set_stage("review", None);
        parked.front.parked_from = Some("review".into());
        parked.front.escalated = true;
        parked.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        parked.save().unwrap();
        let mut gated = queued(&repo, "gated");
        gated.set_stage("implement", None);
        gated.front.paused_at = Some("implement".into());
        gated.set_stage_unbanked(crate::pipeline::PAUSED, "gate");
        gated.save().unwrap();

        for (id, step) in [("parked", "review"), ("gated", "implement")] {
            restart_with(&repo, &Pipelines::builtin(), &restart_args(id), &mux).unwrap();
            let task = queued(&repo, id);
            assert_eq!(task.stage(), step, "{id}");
            assert_eq!(task.front.restart.as_deref(), Some(step), "{id}");
            assert_eq!(task.front.parked_from, None, "{id}");
            assert_eq!(task.front.paused_at, None, "{id}");
            assert!(!task.front.escalated, "{id}");
        }
    }

    /// A hook-held task and a task on a command step have no conversation to
    /// start over. Each is refused, and nothing is written.
    #[test]
    fn restarting_refuses_a_hook_held_or_command_step_task() {
        let (repo, _root_guard) = fixture("restart-no-conversation");
        add(&repo, "hooked", &[]);
        add(&repo, "command", &["hooked"]);
        let mux = RestartMux {
            lanes: Vec::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            on_list: Box::new(|| {}),
            on_stop: Box::new(|| {}),
        };
        let mut hooked = queued(&repo, "hooked");
        hooked.set_stage("implement", None);
        hooked.front.hook_paused = Some("done".into());
        hooked.set_stage_unbanked(crate::pipeline::PAUSED, "held by a hook");
        hooked.save().unwrap();
        let mut command = queued(&repo, "command");
        command.set_stage("handover", None);
        command.save().unwrap();

        let err = restart_with(&repo, &Pipelines::builtin(), &restart_args("hooked"), &mux)
            .unwrap_err()
            .to_string();
        assert!(err.contains("held by a hook"), "{err}");
        let err = restart_with(&repo, &Pipelines::builtin(), &restart_args("command"), &mux)
            .unwrap_err()
            .to_string();
        assert!(err.contains("runs a command"), "{err}");
        assert_eq!(queued(&repo, "hooked").stage(), crate::pipeline::PAUSED);
        assert_eq!(queued(&repo, "hooked").front.restart, None);
        assert_eq!(queued(&repo, "command").front.restart, None);
        assert!(mux.calls().is_empty());
    }

    /// A lane's own environment never restarts a step.
    #[test]
    fn a_lane_cannot_restart_a_step() {
        let (repo, _root_guard) = fixture("restart-from-lane");
        let err = restart(
            &repo,
            &Pipelines::builtin(),
            &restart_args("stuck"),
            Some("implement"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("lane's own environment"), "{err}");
    }

    /// A pipeline `work → mid → after → done`, with `mid` a command step (the
    /// only kind `first:` and `last:` allow) given the `extra` keys that can
    /// hide it, `after` limited to `loop: 1`, and a staffed `blocked`.
    fn walk_past_pipeline(extra: &str) -> Pipeline {
        let yaml = format!(
            "steps:\n  \
             - id: work\n    agent: pi\n    on_pass: mid\n  \
             - id: mid\n    run: 'true'\n    {extra}\n    on_pass: after\n  \
             - id: after\n    agent: pi\n    loop: 1\n    on_pass: done\n  \
             - id: blocked\n    agent: pi\n    session: true\n"
        );
        Pipeline::parse("default", &yaml).unwrap()
    }

    /// Route a `--pass` from `work` for a task the `hide` closure sets up, and
    /// return the task with what `route` decided.
    fn pass_from_work(
        name: &str,
        pipeline: &Pipeline,
        dependents: usize,
        hide: impl FnOnce(&mut Task),
    ) -> (Task, Routed) {
        let (repo, _root_guard) = fixture(name);
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.set_stage("work", None);
        hide(&mut task);
        let routed = route(
            &mut task,
            pipeline,
            "work",
            Outcome::Pass,
            false,
            None,
            dependents,
        )
        .unwrap();
        (task, routed)
    }

    /// Each of the three rules that hide a step sends a pass past it, onto the
    /// step the task runs, and says which rule did it.
    #[test]
    fn a_pass_lands_past_a_step_the_task_walks_past() {
        let (_, skipped) = pass_from_work("route-skip", &walk_past_pipeline(""), 0, |t| {
            t.front.skip = vec!["mid".into()]
        });
        assert_eq!(skipped.destination, "after");
        assert_eq!(
            skipped.walked_past.as_deref(),
            Some("walked past `mid` (skip)")
        );

        let (_, not_root) =
            pass_from_work("route-first", &walk_past_pipeline("first: true"), 0, |t| {
                t.front.depends_on = vec!["parent".into()]
            });
        assert_eq!(not_root.destination, "after");
        assert_eq!(
            not_root.walked_past.as_deref(),
            Some("walked past `mid` (not first in its chain)")
        );

        let (_, not_last) =
            pass_from_work("route-last", &walk_past_pipeline("last: true"), 1, |_| {});
        assert_eq!(not_last.destination, "after");
        assert_eq!(
            not_last.walked_past.as_deref(),
            Some("walked past `mid` (not last in its chain)")
        );
    }

    /// A step that runs for the task is not walked past, and says nothing.
    #[test]
    fn a_pass_onto_a_step_the_task_runs_names_nothing() {
        let (_, routed) =
            pass_from_work("route-runs", &walk_past_pipeline("last: true"), 0, |_| {});
        assert_eq!(routed.destination, "mid");
        assert_eq!(routed.walked_past, None);
    }

    /// The arrival line the report writes keeps the lane's own message and
    /// names the hidden step after it, and the task file never names the
    /// hidden step as a stage.
    #[test]
    fn the_report_writes_the_landing_step_and_names_the_hidden_one_after_the_message() {
        let (repo, _root_guard) = fixture("report-walk-past");
        let mut pipelines = Pipelines::builtin();
        pipelines
            .pipelines
            .insert("default".into(), walk_past_pipeline(""));
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.front.skip = vec!["mid".into()];
        task.set_stage("work", None);
        task.save().unwrap();

        report(
            &repo,
            &pipelines,
            &ReportArgs {
                task: Some("demo".into()),
                stage: None,
                pass: true,
                fail: false,
                block: false,
                pause: false,
                message: Some("all green".into()),
                handoff: Vec::new(),
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), "after");
        assert_eq!(task.rounds_at("mid"), 0, "{:?}", task.front.arrivals);
        assert_eq!(task.rounds_at("after"), 1, "{:?}", task.front.arrivals);
        assert!(
            task.body
                .contains("→ `after`: all green — walked past `mid` (skip)"),
            "{}",
            task.body
        );
        assert!(!task.body.contains("→ `mid`"), "{}", task.body);
    }

    /// The step landed on spends the `loop:`, so one already spent sends the
    /// task to `blocked` — and the walked-past step is named all the same.
    #[test]
    fn a_landing_step_with_its_loop_spent_sends_the_task_to_blocked() {
        let (task, routed) = pass_from_work("route-spent", &walk_past_pipeline(""), 0, |t| {
            t.front.skip = vec!["mid".into()];
            t.front.arrivals.insert("after".into(), 1);
        });
        assert_eq!(routed.destination, crate::pipeline::BLOCKED);
        assert_eq!(
            routed.walked_past.as_deref(),
            Some("walked past `mid` (skip)")
        );
        assert_eq!(task.front.blocked_from.as_deref(), Some("work"));
    }

    /// An unblocker's pass is carried one step past where the task stopped,
    /// and that step is walked past too when it is hidden.
    #[test]
    fn a_cleared_block_lands_past_a_hidden_step() {
        let (repo, _root_guard) = fixture("route-cleared-block");
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.set_stage("work", None);
        task.front.skip = vec!["mid".into()];
        task.front.blocked_from = Some("work".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        let routed = route(
            &mut task,
            &walk_past_pipeline(""),
            crate::pipeline::BLOCKED,
            Outcome::Pass,
            false,
            None,
            0,
        )
        .unwrap();
        assert_eq!(routed.destination, "after");
        assert_eq!(
            routed.walked_past.as_deref(),
            Some("walked past `mid` (skip)")
        );
    }

    /// Several hidden steps in a row are all named, in the order passed.
    #[test]
    fn every_hidden_step_in_a_row_is_named_in_order() {
        let yaml = "steps:\n  \
             - id: work\n    agent: pi\n    on_pass: one\n  \
             - id: one\n    agent: pi\n    on_pass: two\n  \
             - id: two\n    agent: pi\n    on_pass: three\n  \
             - id: three\n    agent: pi\n    on_pass: done\n";
        let pipeline = Pipeline::parse("default", yaml).unwrap();
        let (_, routed) = pass_from_work("route-several", &pipeline, 0, |t| {
            t.front.skip = vec!["one".into(), "two".into()]
        });
        assert_eq!(routed.destination, "three");
        assert_eq!(
            routed.walked_past.as_deref(),
            Some("walked past `one` (skip), `two` (skip)")
        );
    }

    /// Hidden steps wired in a cycle stop after one hop per step in the
    /// pipeline, and a hidden step with no `on_pass` is where the task lands.
    #[test]
    fn the_walk_is_bounded_and_a_hidden_step_with_no_way_on_is_where_it_lands() {
        let task = {
            let (repo, _root_guard) = fixture("walk-bounded");
            add(&repo, "demo", &[]);
            let mut task = queued(&repo, "demo");
            task.front.skip = vec!["a".into(), "b".into(), "end".into()];
            task
        };
        let cycle = Pipeline::parse(
            "default",
            "steps:\n  - id: a\n    agent: pi\n    loop: 5\n    on_pass: b\n  \
             - id: b\n    agent: pi\n    on_pass: a\n",
        )
        .unwrap();
        let landing = crate::dispatch::land_past_hidden(&cycle, &task, "a".into(), 0);
        assert_eq!(landing.passed.len(), cycle.steps.len());

        // A pipeline refuses to parse a step with no `on_pass`, so the dead
        // end is made by taking it away afterwards.
        let mut dead_end = Pipeline::parse(
            "default",
            "steps:\n  - id: a\n    agent: pi\n    on_pass: end\n  \
             - id: end\n    agent: pi\n    on_pass: done\n",
        )
        .unwrap();
        dead_end.steps[1].on_pass = None;
        let landing = crate::dispatch::land_past_hidden(&dead_end, &task, "a".into(), 0);
        assert_eq!(landing.destination, "end");
        assert_eq!(landing.passed.len(), 1);
    }

    /// `spoolway resume --stage` on a step the task walks past moves nothing
    /// and names the rule.
    #[test]
    fn resume_stage_refuses_a_step_the_task_walks_past() {
        let (repo, _root_guard) = fixture("resume-stage-hidden");
        let mut pipelines = Pipelines::builtin();
        pipelines
            .pipelines
            .insert("default".into(), walk_past_pipeline("last: true"));
        add(&repo, "demo", &[]);
        add(&repo, "above", &["demo"]);
        let mut task = queued(&repo, "demo");
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.front.blocked_from = Some("work".into());
        task.save().unwrap();

        let err = resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: Some("mid".into()),
                message: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "`mid` does not run for `demo`: it is not last in its chain. Nothing was resumed."
        );
        assert_eq!(queued(&repo, "demo").stage(), crate::pipeline::BLOCKED);

        let mut task = queued(&repo, "demo");
        task.front.skip = vec!["after".into()];
        task.save().unwrap();
        let err = resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: Some("after".into()),
                message: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "`after` does not run for `demo`: its own skip: names it. Nothing was resumed."
        );
        assert_eq!(
            crate::dispatch::Hidden::NotRoot.refusal(),
            "it is not the root of its chain"
        );
    }

    /// A plain resume whose target the task walks past lands on the step it
    /// runs, and the Status Log names the one passed.
    #[test]
    fn a_resume_lands_past_a_step_the_task_walks_past() {
        let (repo, _root_guard) = fixture("resume-walks-past");
        let mut pipelines = Pipelines::builtin();
        pipelines
            .pipelines
            .insert("default".into(), walk_past_pipeline(""));
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.front.skip = vec!["mid".into()];
        task.front.blocked_from = Some("mid".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), "after");
        assert!(
            task.body
                .contains("→ `after`: unblocked by hand — walked past `mid` (skip)"),
            "{}",
            task.body
        );
        assert!(!task.body.contains("→ `mid`"), "{}", task.body);
    }

    /// A gate answered by a resume that walks past a hidden step answers to
    /// the landing step's `loop:`: with that spent, the task goes to
    /// `blocked` rather than onto the step again.
    #[test]
    fn a_gate_resume_onto_a_landing_step_with_its_loop_spent_blocks() {
        let (repo, _root_guard) = fixture("resume-gate-spent");
        let mut pipelines = Pipelines::builtin();
        pipelines
            .pipelines
            .insert("default".into(), walk_past_pipeline(""));
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.set_stage("work", None);
        task.front.skip = vec!["mid".into()];
        task.front.arrivals.insert("after".into(), 1);
        task.front.paused_at = Some("work".into());
        task.front.paused_by = Some("schedule".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert!(
            task.body.contains("walked past `mid` (skip)"),
            "{}",
            task.body
        );
    }

    /// A gate resume that walks past a hidden step whose own `loop:` is spent
    /// goes to `blocked`, and says it was the passed step that refused.
    #[test]
    fn a_gate_resume_past_a_hidden_step_with_its_loop_spent_blocks() {
        let (repo, _root_guard) = fixture("resume-gate-hidden-spent");
        let mut pipelines = Pipelines::builtin();
        pipelines
            .pipelines
            .insert("default".into(), walk_past_pipeline("loop: 1"));
        add(&repo, "demo", &[]);
        let mut task = queued(&repo, "demo");
        task.set_stage("work", None);
        task.front.skip = vec!["mid".into()];
        task.front.arrivals.insert("mid".into(), 1);
        task.front.paused_at = Some("work".into());
        task.front.paused_by = Some("schedule".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        resume(
            &repo,
            &pipelines,
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();

        let task = queued(&repo, "demo");
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert!(
            task.body
                .contains("may not send this past `mid` a 2nd time"),
            "{}",
            task.body
        );
    }

    /// Checking the budgets of the steps a move passes counts nothing: the
    /// arrival is banked where the stage is written, or a launch that is
    /// refused and retried would count it again on every pass.
    #[test]
    fn checking_a_walked_past_budget_banks_no_arrival() {
        let pipeline = walk_past_pipeline("loop: 2");
        let mut task = Task::parse(
            std::path::PathBuf::from("demo.md"),
            "---\nid: demo\nstage: work\nskip:\n- mid\n---\n",
        )
        .unwrap();
        let mut landing = crate::dispatch::land_past_hidden(&pipeline, &task, "mid".into(), 0);
        assert_eq!(landing.destination, "after");

        let ids = walked_past_budgets(&pipeline, &mut task, &mut landing, "work", false);

        assert_eq!(ids, vec!["mid".to_string()]);
        assert_eq!(task.rounds_at("mid"), 0);
        bank_walked_past(&mut task, &ids);
        assert_eq!(task.rounds_at("mid"), 1);
    }

    /// A task left stopped with an entry under `## Blocker`, as the dispatcher
    /// writes one when a lane stops.
    fn stopped_with_a_blocker_entry(repo: &Repo, id: &str, entry: &str) {
        add(repo, id, &[]);
        let mut task = queued(repo, id);
        task.set_stage("review", None);
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.append_to_section("## Blocker", entry);
        task.save().unwrap();
    }

    /// What a lane reading `## Blocker` finds after `entry`: the lines that
    /// follow it, which must say that stop was cleared and when.
    fn after_the_entry(repo: &Repo, id: &str, entry: &str) -> String {
        let task = queued(repo, id);
        let blocker = task.section("## Blocker").unwrap();
        let entry = entry.trim_end();
        let at = blocker
            .find(entry)
            .expect("the earlier entry is kept whole");
        blocker[at + entry.len()..].to_string()
    }

    /// The local dates on either side of `action`. A mark written just before
    /// midnight carries the earlier date, so a check against a date read only
    /// afterwards would fail on timing alone.
    fn dates_around(action: impl FnOnce()) -> Vec<String> {
        let today = || chrono::Local::now().format("%Y-%m-%d").to_string();
        let before = today();
        action();
        vec![before, today()]
    }

    /// Once a person puts a blocked task back, `## Blocker` says the entry
    /// above is a stop already cleared, and when. Anyone reading the section
    /// can then tell it is not a current blocker. The earlier entry itself is
    /// left exactly as it was written. `dates` is [`dates_around`] the move.
    fn assert_marked_cleared(repo: &Repo, id: &str, entry: &str, dates: &[String]) {
        let after = after_the_entry(repo, id, entry);
        assert!(
            after.to_lowercase().contains("cleared") && dates.iter().any(|d| after.contains(d)),
            "nothing under `## Blocker` marks the entry as a stop already cleared, with the \
             date; what follows it is {after:?}"
        );
    }

    /// `spoolway resume` on a blocked task marks its `## Blocker` entry as past.
    #[test]
    fn resuming_a_blocked_task_marks_its_blocker_entry_as_cleared() {
        let (repo, _root_guard) = fixture("blocker-past-resume");
        let entry = "- API Error: 400 Claude Code 2.1.220 does not support this model\n";
        stopped_with_a_blocker_entry(&repo, "stuck", entry);

        let dates = dates_around(|| {
            resume(
                &repo,
                &Pipelines::builtin(),
                &resume_args("stuck", None),
                None,
            )
            .unwrap();
        });

        assert_eq!(queued(&repo, "stuck").stage(), "review");
        assert_marked_cleared(&repo, "stuck", entry, &dates);
    }

    /// The board's `r` marks a blocked task's `## Blocker` entry as past, as
    /// `spoolway resume` does.
    #[test]
    fn resuming_a_blocked_row_from_the_board_marks_its_blocker_entry_as_cleared() {
        let (repo, _root_guard) = fixture("blocker-past-board-resume");
        let entry = "- API Error: 400 Claude Code 2.1.220 does not support this model\n";
        stopped_with_a_blocker_entry(&repo, "stuck", entry);

        let dates = dates_around(|| {
            resume_held_row(&repo, &Pipelines::builtin(), &resume_args("stuck", None)).unwrap();
        });

        assert_eq!(queued(&repo, "stuck").stage(), "review");
        assert_marked_cleared(&repo, "stuck", entry, &dates);
    }

    /// Restarting a blocked task from the board marks its `## Blocker` entry
    /// as past, as a resume does.
    #[test]
    fn restarting_a_blocked_task_marks_its_blocker_entry_as_cleared() {
        let (repo, _root_guard) = fixture("blocker-past-restart");
        let entry = "- API Error: 400 Claude Code 2.1.220 does not support this model\n";
        stopped_with_a_blocker_entry(&repo, "stuck", entry);
        let mux = RestartMux {
            lanes: Vec::new(),
            calls: std::sync::Mutex::new(Vec::new()),
            on_list: Box::new(|| {}),
            on_stop: Box::new(|| {}),
        };

        let dates = dates_around(|| {
            restart_with(&repo, &Pipelines::builtin(), &restart_args("stuck"), &mux).unwrap();
        });

        assert_eq!(queued(&repo, "stuck").stage(), "review");
        assert_marked_cleared(&repo, "stuck", entry, &dates);
    }

    /// A task stopped again after it was put back shows the new entry as the
    /// current one: the mark for the first stop sits between the two entries,
    /// and nothing marks the second.
    #[test]
    fn a_task_stopped_again_after_being_put_back_shows_its_new_entry_as_current() {
        let (repo, _root_guard) = fixture("blocker-past-stopped-again");
        let first = "- API Error: 400 Claude Code 2.1.220 does not support this model\n";
        let second = "- llama-server unreachable\n";
        stopped_with_a_blocker_entry(&repo, "stuck", first);
        let dates = dates_around(|| {
            resume(
                &repo,
                &Pipelines::builtin(),
                &resume_args("stuck", None),
                None,
            )
            .unwrap();
        });

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.append_to_section("## Blocker", second);
        task.save().unwrap();

        assert_marked_cleared(&repo, "stuck", first, &dates);
        let after_first = after_the_entry(&repo, "stuck", first);
        assert!(
            after_first.trim_end().ends_with(second.trim_end()),
            "the new entry is the last thing under `## Blocker`: {after_first:?}"
        );
        let after_second = after_the_entry(&repo, "stuck", second);
        assert!(
            after_second.trim().is_empty(),
            "nothing marks the new entry as past: {after_second:?}"
        );
    }

    /// A park taken on a blocked task goes back onto `blocked` with its stop
    /// still standing, so its newest `## Blocker` entry is not marked past.
    #[test]
    fn resuming_a_park_taken_on_a_blocked_task_leaves_its_blocker_entry_current() {
        let (repo, _root_guard) = fixture("blocker-park-on-blocked");
        let entry = "- timeout\n";
        stopped_with_a_blocker_entry(&repo, "stuck", entry);
        let mut task = queued(&repo, "stuck");
        task.front.parked_from = Some(crate::pipeline::BLOCKED.into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        resume(
            &repo,
            &Pipelines::builtin(),
            &resume_args("stuck", None),
            None,
        )
        .unwrap();

        assert_eq!(queued(&repo, "stuck").stage(), crate::pipeline::BLOCKED);
        let after = after_the_entry(&repo, "stuck", entry);
        assert!(
            after.trim().is_empty(),
            "the live stop is marked: {after:?}"
        );
    }

    /// A task held at a gate over a block, as `spoolway resume` finds one: it
    /// paused at `deploy` with `blocked_from` naming it, and has an entry
    /// under `## Blocker`.
    fn gated_over_a_block(repo: &Repo, id: &str, entry: &str, report_outcome: Option<&str>) {
        add(repo, id, &[]);
        let mut task = queued(repo, id);
        task.set_stage("deploy", None);
        task.front.paused_at = Some("deploy".into());
        task.front.blocked_from = Some("deploy".into());
        task.front.last_report = report_outcome.map(|outcome| crate::task::LastReport {
            step: "deploy".into(),
            outcome: outcome.into(),
            at: 0,
            blocked: false,
        });
        task.set_stage(crate::pipeline::PAUSED, None);
        task.append_to_section("## Blocker", entry);
        task.save().unwrap();
    }

    /// Answering a gate that stood over a block clears that block, so the
    /// gate road marks `## Blocker` as the other roads do.
    #[test]
    fn clearing_a_block_held_at_a_gate_marks_its_blocker_entry_as_cleared() {
        let (repo, _root_guard) = fixture("blocker-past-gate-cleared");
        let entry = "- deploy window closed\n";
        gated_over_a_block(&repo, "ship", entry, None);

        let dates = dates_around(|| {
            resume(&repo, &gate_pipelines(), &resume_args("ship", None), None).unwrap();
        });

        assert_ne!(queued(&repo, "ship").stage(), crate::pipeline::BLOCKED);
        assert_marked_cleared(&repo, "ship", entry, &dates);
    }

    /// A gate answered onto `blocked` leaves the stop standing, so its entry
    /// is not marked past.
    #[test]
    fn answering_a_gate_onto_blocked_leaves_its_blocker_entry_current() {
        let (repo, _root_guard) = fixture("blocker-gate-onto-blocked");
        let entry = "- deploy window closed\n";
        gated_over_a_block(&repo, "ship", entry, Some("block"));

        resume(&repo, &gate_pipelines(), &resume_args("ship", None), None).unwrap();

        assert_eq!(queued(&repo, "ship").stage(), crate::pipeline::BLOCKED);
        let after = after_the_entry(&repo, "ship", entry);
        assert!(
            after.trim().is_empty(),
            "the live stop is marked: {after:?}"
        );
    }
}
