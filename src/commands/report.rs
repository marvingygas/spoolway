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
    let current = task.stage().to_string();

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

    let routed = route(
        &mut task,
        pipeline,
        &current,
        outcome,
        unattended,
        args.stage.as_deref(),
    )?;
    let mut destination = routed.destination;
    let gated = routed.gated;
    let pause_note = routed.pause_note;
    let resumed = routed.resumed;
    let paused_from_blocked = routed.paused_from_blocked;

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
    });

    task.set_stage(
        &destination,
        pause_note.as_deref().or(args.message.as_deref()),
    );
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
        None if gated => println!(
            "{id}: {current} --{outcome}--> {} - held here for a person{}",
            crate::pipeline::PAUSED,
            stop_choices(&id, &current)
        ),
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
    /// The status-log wording a gate wants in place of the lane's own `-m`
    /// message, if one caught this report.
    pub pause_note: Option<String>,
    /// The step this run resumed itself onto, when nobody was staffing
    /// `blocked` to park in front of. `None` on every other road out.
    pub resumed: Option<String>,
    /// Whether `current` was `blocked` itself and the outcome was anything
    /// but a pass — the one case with no second destination, parked on
    /// `paused` instead. [`report`]'s own final message tells this apart
    /// from an ordinary park.
    pub paused_from_blocked: bool,
}

/// The routing decision, lifted out of [`report`] so a test can walk every
/// outcome at every step through the same function `spoolway report` itself
/// calls — no `Repo`, no filesystem, no clock, so the walk that proves it
/// bounded can run entirely in `cargo test`.
///
/// `task` is mutated in place: routing is not only a lookup, it is also
/// where a cleared block gives a lane back its budgets (`resume_at`), where
/// a spent loop logs why it gave up (`apply_loop_budget`), and where a gate
/// stamps `paused_at`/`paused_by`. All of that belongs to the routing
/// decision and moves with it — only the parts of [`report`] that need a
/// real repository (committing the worktree, saving the file, firing the
/// tracking hook) stay behind.
pub fn route(
    task: &mut Task,
    pipeline: &Pipeline,
    current: &str,
    outcome: Outcome,
    unattended: bool,
    stage: Option<&str>,
) -> Result<Routed> {
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

    let mut destination = if current == crate::pipeline::BLOCKED && outcome == Outcome::Pass {
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
        // `spoolway resume` performs by hand, minus the one thing only a
        // person may do: the loop budgets out of the step the task stopped on
        // stay spent (`by_hand: false`), because this is the run clearing its
        // own block and a budget it refunds to itself bounds nothing. What it
        // does share is the mark: the lane that originally hit the block —
        // which had often already read the tree and done most of the work —
        // is continued rather than replaced by a cold one.
        //
        // `stage` overrides `cleared_block_target`'s own answer when the
        // unblocker names one — bounded by `steps_run`, the steps this task
        // has actually run a lane at, so a `--stage` can send it back onto
        // ground already covered but never somewhere it was never staffed.
        let target = match stage {
            Some(named) => {
                let run = steps_run(task, pipeline);
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
            None => cleared_block_target(task, pipeline, true),
        };
        resume_at(task, pipeline, &target, false);
        target
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
        let origin = task
            .front
            .blocked_from
            .clone()
            .unwrap_or_else(|| resume_target(task, pipeline));
        task.front.paused_at = Some(origin);
        crate::pipeline::PAUSED.to_string()
    } else {
        step.destination(outcome)
            .unwrap_or(crate::pipeline::BLOCKED)
            .to_string()
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
    // pass, and a task's own `gate_at`, set by whoever wrote its document to
    // hold this one task without giving it a pipeline of its own — and which
    // catches this step's outcome whatever it was, `destination` already
    // carrying whatever `apply_loop_budget` and the `blocked` check above made
    // of it. `spoolway resume` is what tells a caught pass from a caught fail
    // or block apart again, from `last_report` and `blocked_from` — see
    // `past_the_gate`.
    let hold = gate_hold(task, step, outcome, &destination);
    let gated = hold.is_some();
    // What the status log says about this arrival, in place of the lane's own
    // `-m` message — the Mockup draws the gate note alone, and this is that
    // wording change. The lane's own account of the pass genuinely does not
    // reach this log line any more: a lane leaving something for the person
    // who answers the gate to read has `--handoff` for it, credited to
    // `current` in `## Handoff` above, same as any other step — the `-m`
    // message itself is not copied there automatically, so a lane that wants
    // both has to say so with `--handoff` too.
    let mut pause_note = None;
    if let Some(kind) = hold {
        task.front.paused_at = Some(current.to_string());
        task.front.paused_by = Some(kind.as_str().to_string());
        destination = crate::pipeline::PAUSED.to_string();
        pause_note = Some(match kind {
            Gate::Schedule => "held by this task's own schedule".to_string(),
            Gate::Step => "held by this step's own gate".to_string(),
        });
        // Spent, not standing, whoever wrote it — the board's `s` is the
        // example, but a `gate_at` typed by hand into the document fires and
        // clears exactly the same way. A step's own `gate: true` is the one
        // that holds every task that ever reaches it. Left set, a later
        // route that brought this task back onto the same step — a loop, a
        // `--stage` reroute — would gate it a second time nobody asked for.
        if kind == Gate::Schedule {
            task.front.gate_at = None;
        }
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
        // The run resuming itself, so the budgets stay spent — see
        // `resume_at`. A spent budget never arrives here in the first place:
        // `apply_loop_budget` skips a limit whose exit is `blocked` in exactly
        // this configuration, rather than handing the task a wall it can only
        // walk into again.
        resume_at(task, pipeline, &target, false);
        destination = target.clone();
        resumed = Some(target);
    }

    Ok(Routed {
        destination,
        gated,
        pause_note,
        resumed,
        paused_from_blocked,
    })
}

/// A loop that has gone round too many times escalates instead of going round
/// again, rather than a task landing on `destination` unconditionally.
/// Counting is what makes this a lookup rather than a judgement call about
/// whether progress is being made — and it counts this route in, so a task
/// that spent its review-fix budget still gets its e2e-fix one.
///
/// The budget belongs to the step making the move, not the one receiving it:
/// `loop: { implement: 2 }` on `review` is read here as "`review` may send
/// this back to `implement` twice", so it is `review`'s third *failure* that
/// takes `review`'s own `loop_exit()`. Bounding the arrival instead — which
/// is what this did — spent the budget one edge too early: the third passing
/// `implement` was redirected before `review` had seen the fix it was
/// reporting, and `on_loop_max` then answered for a step that never ran.
/// Nothing about a pipeline file changed; the same `from -> to` counter in
/// [`Task::rounds_via`] is read off the other end of it.
///
/// Shared by [`report`], where a lane's own account of itself proposes
/// `destination`, and the dispatcher's command-step routing, where a `run:`
/// step's exit code does — a spent loop stops circling the same way whichever
/// kind of step is asking, and a mechanical gate failing back to the agent
/// step behind it is exactly the second case.
///
/// What it counts is laps, not conversations: a step with `session: true` may
/// be re-prompted as often as its session survives, with an optional separate
/// bound — `session_reuse_ctx` on the agent profile.
///
/// An unattended run still takes this exit. `on_loop_max` is where the
/// pipeline said the loop goes, and most of what it can say needs nobody: the
/// default carries the task on to `on_pass` with its findings attached, which
/// is a destination rather than a request for a person, and `on_loop_max:
/// handover` reads the same way. The one reading that does need one is an
/// exit resolving to `blocked` — and in a run with nobody staffing `blocked`,
/// that exit is answered by the run itself, which sends the task straight
/// back to the step it stopped on. The budget would then be spent again on
/// the very next transition, and every one after it, with nobody to clear it:
/// the same loop, one lane more expensive per lap. So that one reading is
/// skipped here, and the task file does not fill with rounds bought by a
/// wall the run can only walk into. [`resume_at`] no longer refunds anything
/// for either self-resume, so this is the whole of the carve-out.
pub fn apply_loop_budget(
    pipeline: &Pipeline,
    task: &mut Task,
    current: &str,
    destination: String,
    unattended: bool,
) -> String {
    let Some(step) = pipeline.step(current) else {
        return destination;
    };
    let exit = step.loop_exit().to_string();
    let limit = step.round_limit(&destination).filter(|_| {
        !unattended || exit != crate::pipeline::BLOCKED || pipeline.blocked_is_staffed(unattended)
    });
    if limit.is_some_and(|limit| task.rounds_via(current, &destination) >= limit) {
        // The move it is not making, counted the way a reader counts: the
        // budget is spent, so the one being refused is the next one after it.
        let note = format!(
            "`{current}` may not send this back to `{destination}` a {} time — carrying on to \
             `{exit}`",
            ordinal(limit.unwrap_or(0) + 1)
        );
        task.log_status(&note);
        return exit;
    }
    destination
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
/// when its step settles, so that a cleanup terminal cannot delete work
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

/// Which of the two roads holds a task at `step` for `outcome`/`destination`
/// — the one predicate every reader of a gate now shares, rather than each
/// deriving its own copy. `compose::report_contract` asks it of a
/// hypothetical pass, before a lane has run; [`report`] asks it of the real
/// outcome and the destination it just resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// A task's own `gate_at`, set by whoever wrote its document — catches
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
/// roads [`report`] itself parks a pass in front of a person for, carrying
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
/// unblocker's lane starts (`start_one`, the same as any other step), so
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
    /// pipeline drew. Resuming still takes `on_pass`: taking that verdict is
    /// why the pause was scheduled.
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
/// answer once it is set: `report` never writes it without also filing
/// `last_report` from `gated` in the same save, so trusting it here rather
/// than re-checking `last_report.step` against `gated` changes nothing for
/// a task this task ever paused. A task already sitting on `paused` from
/// before `paused_by` existed carries none, so the fallback this was built
/// on outright — `last_report.step == gated`, filed only by a road that
/// actually caught something — still answers for it.
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

/// Set a stopped task up to carry on from `target`, and say which loop budgets
/// that cost.
///
/// The whole of what resuming *is*, in one place, because there are now four
/// callers who must agree to the letter: `spoolway resume` by hand, a pass
/// reported from `blocked`, a lane's own report in an unattended run, and the
/// dispatcher's own escalation in one. A version of this that drifted between
/// them would be a task that resumes and then stops again on the next
/// transition, or one whose second lane starts cold on work the first had
/// already finished.
///
/// Two things happen, and both are about not repeating work:
///
/// The loop budgets out of the step the task *stopped on* are handed back —
/// but only when `by_hand`, which is to say only when a person asked. Resuming
/// a task that ran out of rounds otherwise buys it one attempt and then stops
/// it again on the very next transition — not a resume, the same wall one step
/// further along. Only the routes out of that step, and only the ones the step
/// itself bounds: a stuck review loop being let go says nothing about the
/// rebase loop at the other end of the pipeline, and the counters it never
/// spent are worth keeping.
///
/// The other two callers are the run resuming *itself* — a pass reported from
/// `blocked`, and the unattended self-resume for an unstaffed `blocked` — and
/// they hand nothing back. A licence a run extends to itself is not a licence:
/// a budget refunded on every block is a budget that never runs out, which is
/// exactly the unbounded loop `loop:` exists to close. Only a person deciding
/// that this loop deserves another go extends it.
///
/// The step it stopped on, rather than `target`, because a `--pass` from
/// `blocked` on an agent step carries `target` one step past there — see
/// `cleared_block_target` — so those are no longer the same step. A task that
/// spent `e2e → test` and blocked resumes at `test`, and handing back the
/// budgets out of `test` would return counters nothing spent while leaving the
/// spent one in place — so the first failure at `test` would route to `e2e`,
/// find the wall still there, and block again immediately. `blocked_from` is
/// read before it is cleared below; the two other callers pass a `target` equal
/// to it, so nothing changes for them.
///
/// Only `rounds` is handed back: that is what a budget is spent from.
/// `steps` is the record of what this task has cost, and returning it would
/// rewrite history rather than extend a licence.
///
/// And the lane that stopped is marked to be *continued* rather than replaced,
/// when the task is going back to where it actually stopped. Whatever was in
/// the way was outside that lane's control — that is what a block is — so its
/// work stands, and it is often work that was already finished when it stopped.
/// A task being sent somewhere else has been rerouted rather than resumed, and
/// the lane at the far end has nothing to say about why it stopped — which is
/// the case a take-over lands in every time, and correctly: the step it is
/// carried to has a prompt of its own that was never in the room.
pub fn resume_at(task: &mut Task, pipeline: &Pipeline, target: &str, by_hand: bool) -> Vec<String> {
    let origin = task
        .front
        .blocked_from
        .clone()
        .unwrap_or(target.to_string());
    let mut returned: Vec<String> = Vec::new();
    if by_hand && let Some(step) = pipeline.step(&origin) {
        // The step's own `loop:`, not the destination's: a budget is spent by
        // whoever makes the move, so the counter to hand back is the one
        // `apply_loop_budget` read on the way out of this very step.
        for next in pipeline.destinations(step) {
            if step.round_limit(next).is_some()
                && task
                    .front
                    .rounds
                    .remove(&route_key(&origin, next))
                    .is_some()
            {
                returned.push(format!("{origin} → {next}"));
            }
        }
    }

    if task.front.blocked_from.as_deref() == Some(target) {
        task.front.resume = Some(target.to_string());
    }
    task.front.blocked_from = None;
    returned
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
/// approved by a lane.
pub fn resume(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &ResumeArgs,
    from_step: Option<&str>,
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
    let task = repo.task(&args.task)?;
    if from_blocked && task.front.paused_at.is_some() {
        bail!(
            "task `{}` is waiting on a gate — that is a person's to answer, not a lane's.",
            args.task
        );
    }

    // A gate is the one road out of `paused` that answers a *question*
    // rather than a stop — `paused_at` is what tells the two apart, not the
    // stage alone: a `p` park and a resumed block both land on `paused` too,
    // and neither has a gate to answer. Checked ahead of `--stage`, which a
    // person names to reroute a genuine gate on purpose.
    match task.front.paused_at.is_some() && args.stage.is_none() {
        true => past_the_gate(pipelines, task, args),
        false => back_onto_its_step(repo, pipelines, task, args),
    }
}

fn back_onto_its_step(
    repo: &Repo,
    pipelines: &Pipelines,
    mut task: Task,
    args: &ResumeArgs,
) -> Result<()> {
    // A `p` park is answered differently from a real stop: nothing was ever
    // in the way, so putting it back is not a lap and runs none of
    // `resume_at`'s bookkeeping — see `unpark`. Only when nobody has
    // overridden the route by hand: `--stage` on a parked task is a person
    // choosing to reroute it, which is exactly what the ordinary path below
    // is for.
    if args.stage.is_none() && task.front.parked_from.is_some() {
        return unpark(repo, pipelines, task);
    }

    let pipeline = pipelines.for_task(&task)?;

    let target = match &args.stage {
        Some(stage) => {
            pipeline.require_step(stage)?;
            stage.clone()
        }
        None => resume_target(&task, pipeline),
    };

    let message = args
        .message
        .clone()
        .unwrap_or_else(|| "unblocked by hand".to_string());
    let returned = resume_at(&mut task, pipeline, &target, true);
    // Whatever gate it was waiting on, it is not waiting on it here any more.
    task.front.paused_at = None;
    task.front.paused_by = None;
    // A `--stage` reroute past a park leaves this ordinary road instead of
    // `unpark`'s, but the park is answered all the same — left set, this
    // would still name the step on a later, ordinary retry, and `start_one`
    // would read that retry as a continued park rather than what it is.
    task.front.parked_from = None;
    task.front.escalated = false;
    task.set_stage(&target, Some(&message));
    task.save()?;

    for route in &returned {
        println!("  gave back the rounds on {route}");
    }

    free_stale_lanes(repo, pipelines, &task);

    println!("{}: -> {target}", args.task);
    Ok(())
}

/// Put a `parked_from` task back on the step it never left — the road
/// `back_onto_its_step` takes instead of `resume_at` when nothing actually
/// failed a check: a person's own keypress or Escape, or a lane
/// `escalate_clock` gave up on. One code path either way in: the board's
/// `enter` and `R` call this same `resume` with no `--stage` of their own,
/// the same as a bare `spoolway resume <task>` does, and `back_onto_its_step`
/// is what finds `parked_from` and lands here.
///
/// `parked_from` (and `escalated` beside it) are left in the task file rather
/// than cleared here — the launch that actually continues this step is what
/// learns whether a session was there to carry, and `Dispatcher::start_one`
/// in `src/dispatch.rs` is what spends both once that answer is known, the
/// same moment it spends `resume`.
fn unpark(repo: &Repo, pipelines: &Pipelines, mut task: Task) -> Result<()> {
    let step = task
        .front
        .parked_from
        .clone()
        .expect("checked by back_onto_its_step before calling unpark");
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

/// This task's settled lanes freed, so the dispatcher does not mistake one
/// left over from before a stop for a lane still waiting on an answer — the
/// task would wait on it forever and no fresh lane would ever start. Shared
/// by `back_onto_its_step` and `unpark`, the two roads that put a task back
/// on a step a lane of its own might already be sitting settled on.
///
/// Only this task's settled lanes, and only ones working in this project's
/// directories — the same ownership rules a pass applies. Busy ones are left
/// alone: work that is genuinely running is the dispatcher's to watch, not
/// ours to kill.
fn free_stale_lanes(repo: &Repo, pipelines: &Pipelines, task: &Task) {
    let Ok(mux) = crate::mux::backend(repo) else {
        return;
    };
    if let Ok(lanes) = mux.list_lanes() {
        let step_ids = pipelines.all_step_ids();
        for lane in lanes {
            let ours =
                lane.cwd == repo.root || Some(&lane.cwd) == task.front.worktree_path.as_ref();
            let this_task = crate::mux::parse_lane_name(&lane.name, &step_ids)
                .is_some_and(|(_, task_id)| task_id == task.front.id);
            if ours && this_task && lane.status.is_settled() {
                let _ = mux.stop_lane(&lane.name, &lane.pane_id);
                println!("  freed stale lane `{}`", lane.name);
            }
        }
    }
}

/// The person's half of a gate: let a paused task past its gated step, or send
/// it back round.
///
/// This is the answer `crate::pipeline::Step::gate` waits for. The task's work
/// is done and committed — a gated lane reports like any other, and its report
/// went through [`report`] in full — so nothing here runs, rebuilds or checks
/// anything. All that is left is the routing decision the pass was not allowed
/// to take on its own.
///
/// Where it goes is read out of the pipeline *now*, from the step recorded in
/// `paused_at`, rather than out of anything the pass wrote down. A pipeline
/// edited while a task sat on `paused` should route the task the way the file
/// says today; a destination frozen at report time would send it somewhere the
/// project has since stopped meaning.
fn past_the_gate(pipelines: &Pipelines, mut task: Task, args: &ResumeArgs) -> Result<()> {
    let pipeline = pipelines.for_task(&task)?;

    // The step it paused on, which is the only thing that says where "on" is.
    // A task file hand-edited onto `paused` has none, and there is nothing to
    // guess: the pipeline's entry would restart work that is already done.
    let gated = task.front.paused_at.clone().with_context(|| {
        format!(
            "task `{}` is paused but records no step it paused at, so nothing here knows what \
             it was waiting to be let past. `spoolway resume {} --stage <step>` puts it back \
             on a step by name.",
            args.task, args.task
        )
    })?;
    let step = pipeline.require_step(&gated).with_context(|| {
        format!(
            "task `{}` paused at `{gated}`, which pipeline `{}` no longer defines",
            args.task, pipeline.name
        )
    })?;

    // What this pause actually caught — `None` for a pause raised from
    // `blocked` itself, which is not a catch of anything. See [`caught_at`].
    let caught = caught_at(&task, &gated);

    // A pause raised from `blocked` itself, told apart from an intercepted
    // catch at `gated` by `caught` above: both leave `blocked_from` naming
    // the very step `paused_at` does, but only `blocked`'s own road leaves no
    // report filed from `gated` to read. Its own pass never runs `blocked`'s
    // absent `on_pass` — it takes `blocked_from`'s, through the same
    // `cleared_block_target` a pass from `blocked` reads — so accepting it
    // here has to reach exactly there too, rather than the plain `on_pass`
    // below, which is what an ordinary gate means and is not what a person
    // clearing this one is answering.
    let cleared_block =
        caught.is_none() && task.front.blocked_from.as_deref() == Some(gated.as_str());

    let destination = if cleared_block {
        let target = cleared_block_target(&task, pipeline, false);
        resume_at(&mut task, pipeline, &target, true);
        target
    } else if caught == Some(Caught::Blocked) {
        // What `set_blocked_from` already ran for on the way here — a
        // `--block`, a step's own `on_fail: blocked`, or a spent loop's own
        // exit — a plain `resume` sends exactly where it would have landed
        // unheld.
        crate::pipeline::BLOCKED.to_string()
    } else {
        step.destination(Outcome::Pass)
            .unwrap_or(crate::pipeline::BLOCKED)
            .to_string()
    };

    // `blocked_from` naming `gated` stops describing where this task is
    // stopped the moment it moves anywhere but `blocked` itself — left
    // standing, it would outlive this answer and read as a caught block the
    // next time this same step is gated and passes cleanly (review finding
    // 4). Only the branch above that actually sends the task to `blocked`
    // still needs it; `resume_at`, for `cleared_block`, already clears it on
    // its own road.
    if destination != crate::pipeline::BLOCKED
        && task.front.blocked_from.as_deref() == Some(gated.as_str())
    {
        task.front.blocked_from = None;
    }

    let note = args.message.clone().unwrap_or_else(|| match cleared_block {
        true => format!("block cleared by hand; nothing was done at `{gated}`"),
        false => format!("`{gated}` released at the gate"),
    });

    task.front.paused_at = None;
    task.front.paused_by = None;
    task.set_stage(&destination, Some(&note));
    task.save()?;

    // `destination` always equals `gated` here — `cleared_block_target` with
    // `takes_over: false` hands the task straight back to the step it
    // blocked on — so the plain `{gated} --pass--> {destination}` phrasing
    // below would print a step arrowing to itself, reading like a pass that
    // ran and landed nowhere rather than a block being cleared.
    if cleared_block {
        println!("{}: {gated}: block cleared by hand", args.task);
    } else {
        // `resume` rather than `outcome`'s own `pass`: this names the
        // person's answer, while the label retains the outcome the schedule
        // caught. A plain gated pass still reads as it always has.
        let label = match caught {
            Some(Caught::Blocked) => format!("{gated} {}", crate::pipeline::BLOCKED),
            Some(Caught::Fail) => format!("{gated} failed"),
            _ => gated.clone(),
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

    /// `cargo test` for this whole crate runs in one process, and when that
    /// process is itself a lane's own `test` step, the real dispatcher has
    /// already exported `SPOOLWAY_TASK`, `SPOOLWAY_WORKTREE` and
    /// `SPOOLWAY_HEAD` for *that* lane before a single `#[test]` runs. Left
    /// alone, every fixture task id below — `stuck`, `task-1` and the rest —
    /// would be compared against, or committed alongside, the real lane's own
    /// task rather than a clean slate. Cleared once, here, rather than by
    /// every test re-clearing what it never set in the first place; a test
    /// that needs one of the three set for real sets it itself, on the
    /// fixture id it is actually testing.
    fn clear_lane_env() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for key in ["SPOOLWAY_TASK", "SPOOLWAY_WORKTREE", "SPOOLWAY_HEAD"] {
                crate::platform::remove_test_env(key);
            }
        });
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
        clear_lane_env();
        let repo = fixture("auto-commit");
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
        clear_lane_env();
        let repo = fixture("report-wrong-task");
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
        clear_lane_env();
        let mut repo = fixture("auto-commit-off");
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
        clear_lane_env();
        let repo = fixture("report-leaves-verdict");
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
        clear_lane_env();
        let repo = fixture("report-carries-handoff");
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
        clear_lane_env();
        let repo = fixture("block-records-step");
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
    fn unattended_fixture(name: &str) -> Repo {
        let mut repo = fixture(name);
        repo.config.unattended.enabled = true;
        repo
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
        clear_lane_env();
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
        let repo = fixture("report-from-a-stale-lane");
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

    /// A pass that would route to a cleanup terminal is held at `blocked`
    /// while the lane's worktree still has uncommitted work in it —
    /// `auto_commit` off here, standing in for the git failure it now reports
    /// rather than swallows — so a `done` teardown cannot delete work that was
    /// never recorded (review finding 4).
    #[test]
    fn a_pass_to_a_cleanup_terminal_is_held_while_the_worktree_is_dirty() {
        clear_lane_env();
        let mut repo = fixture("held-dirty");
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
        let repo = unattended_fixture("unattended-roundtrip");
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
        let repo = unattended_fixture("staffed-blocked-pass");
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
        let repo = unattended_fixture("staffed-blocked-command-pass");
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
        let repo = unattended_fixture("staged-pass-lands");
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

        clear_lane_env();
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

    /// The other half of the mockup: naming a step this task has never run a
    /// lane at is refused, and the refusal names exactly the steps `steps:`
    /// says it has — in pipeline order, not the map's own key order.
    #[test]
    fn a_staged_pass_naming_a_step_never_run_is_refused() {
        let repo = unattended_fixture("staged-pass-refused");
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

        clear_lane_env();
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

    /// `--stage` is bounded to `--pass` off `blocked` itself, the same as
    /// `--pause` — named and refused rather than silently ignored off any
    /// other step.
    #[test]
    fn a_staged_pass_is_refused_off_blocked() {
        let repo = unattended_fixture("staged-pass-off-blocked");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "confirm-dialog", &[]);
        let pipelines = three_step_staffed_pipelines();

        let mut task = queued(&repo, "confirm-dialog");
        task.set_stage("implement", None);
        task.save().unwrap();

        clear_lane_env();
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
        let repo = unattended_fixture("staged-pass-excludes-blocked");
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
        // what `start_one` does for any staffed step, `blocked` included.
        task.bank_launch("look", crate::pipeline::BLOCKED);
        task.save().unwrap();

        clear_lane_env();
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
        let repo = fixture("blocked-hands-back-block");
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
        let repo = fixture("blocked-hands-back-pause");
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
        let repo = unattended_fixture("lost-origin");
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = None;
        task.front.last_report = Some(crate::task::LastReport {
            step: "work".into(),
            outcome: "block".into(),
            at: 1,
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
        let repo = unattended_fixture("blocked-last-report");
        let pipelines = staffed_pipelines();
        let pipeline = pipelines.pipelines.get("default").unwrap();
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = None;
        task.front.last_report = Some(crate::task::LastReport {
            step: crate::pipeline::BLOCKED.into(),
            outcome: "block".into(),
            at: 1,
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

    /// A fail, a block or a pause from that same staffed step never lands back
    /// on `blocked` itself any more — there is no escalation past it, so it
    /// parks on `paused` instead, naming the step it originally blocked on and
    /// keeping `blocked_from` for a later resume to read.
    #[test]
    fn a_staffed_blocked_steps_fail_block_or_pause_lands_on_paused() {
        for outcome in [Outcome::Fail, Outcome::Block, Outcome::Pause] {
            let repo = unattended_fixture(&format!("staffed-blocked-{outcome}"));
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
        clear_lane_env();
        let repo = fixture("blocked-pause-resume");
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
        let repo = fixture("pause-elsewhere");
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
        let repo = unattended_fixture("unattended-fail");
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

    /// A spent budget whose exit is `blocked` is a request for a person, on a
    /// pipeline that does not stage `blocked` itself. `review`'s own `loop:`
    /// carries no `on_loop_max:`, so its default exit is its own `on_pass` —
    /// `document`, not `blocked` — so this test gives it `on_loop_max:
    /// blocked` by hand, the one case worth testing here. It also strips the
    /// shipped `blocked` step so that exit is unstaffed. Unattended there is
    /// no person to hand either of those to, only the run's own resume
    /// straight back onto `review` — which hands nothing back, so the same
    /// wall would be hit on every transition after this one. The bound is
    /// skipped outright instead, and the task file does not fill up with
    /// rounds bought against a wall nothing can clear.
    #[test]
    fn a_budget_bound_for_a_person_does_not_bind_an_unattended_run_with_no_staffed_blocked_step() {
        let repo = unattended_fixture("unattended-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // The shipped `default` pipeline, minus its `blocked` step — the case
        // this test is about, and the shape every project's pipeline had
        // before this one could declare it. `review`'s own `loop:` carries no
        // `on_loop_max:`, so its exit is `review`'s own `on_pass` — `document`
        // — which is not `blocked` either; swap it in for this test alone so
        // the exit really is the default `blocked`.
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
            let review = pipeline
                .steps
                .iter_mut()
                .find(|s| s.id == "review")
                .unwrap();
            review.on_loop_max = Some(crate::pipeline::BLOCKED.to_string());
        }

        // Spent right up to `review`'s limit on the route back to `implement`:
        // attended, the next failure is where the task stops.
        let spent = review_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.rounds.insert("review->implement".into(), spent);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "implement",
            "the loop goes round rather than stopping"
        );
        assert_eq!(
            task.rounds_via("review", "implement"),
            spent + 1,
            "the bound is skipped outright, not spent and refunded, so this move banks a \
             round exactly as an unbounded one would"
        );
    }

    /// The same spent budget, on the shipped pipeline as it actually ships —
    /// staffing `blocked` with its own sample prompt, and `review` naming no
    /// `on_loop_max:` of its own so its default exit is `document`, which is
    /// not `blocked` — so this test names `on_loop_max: blocked` by hand, the
    /// one case where an exit resolving to a staffed `blocked` is a real
    /// destination again, staffed by a lane rather than a person, and the
    /// limit binds exactly as it would in an attended run.
    #[test]
    fn a_budget_bound_does_bind_an_unattended_run_that_staffs_blocked() {
        let repo = unattended_fixture("unattended-rounds-staffed");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            let review = pipeline
                .steps
                .iter_mut()
                .find(|s| s.id == "review")
                .unwrap();
            review.on_loop_max = Some(crate::pipeline::BLOCKED.to_string());
        }

        let spent = review_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.rounds.insert("review->implement".into(), spent);
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
        let repo = fixture("attended-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            let review = pipeline
                .steps
                .iter_mut()
                .find(|s| s.id == "review")
                .unwrap();
            review.on_loop_max = Some(crate::pipeline::BLOCKED.to_string());
        }

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front
            .rounds
            .insert("review->implement".into(), review_limit(&pipelines));
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);
        assert_eq!(queued(&repo, "stuck").stage(), "blocked");
    }

    /// The whole of what a budget counts, walked end to end on the pipeline
    /// that ships: `review` carries `loop: { implement: 2 }`, so it may send
    /// this back twice, and it is the *third failure* that takes the exit.
    /// The third arrival is not it, and that is the point — a judging step
    /// has to see the fix it asked for, and bounding arrivals redirected the
    /// third passing `implement` before `review` ever read the work.
    #[test]
    fn the_third_failure_takes_the_exit_and_the_third_arrival_still_lands() {
        let repo = fixture("three-arrivals");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "spinner", &[]);

        let pipelines = Pipelines::builtin();
        assert_eq!(
            review_limit(&pipelines),
            2,
            "the shipped budget this walks lap by lap"
        );

        let mut task = queued(&repo, "spinner");
        task.set_stage("implement", None);
        task.save().unwrap();

        for lap in 1..=2 {
            report_outcome(&repo, &pipelines, "spinner", Outcome::Pass);
            assert_eq!(
                queued(&repo, "spinner").stage(),
                "review",
                "arrival {lap} at `review` is not what the budget bounds"
            );
            report_outcome(&repo, &pipelines, "spinner", Outcome::Fail);
            assert_eq!(
                queued(&repo, "spinner").stage(),
                "implement",
                "`review` may still make backward move {lap}"
            );
        }

        // The third arrival, with the budget already spent: it still lands.
        report_outcome(&repo, &pipelines, "spinner", Outcome::Pass);
        let task = queued(&repo, "spinner");
        assert_eq!(
            task.stage(),
            "review",
            "a passing `implement` reaches `review` however spent the budget is"
        );
        assert_eq!(task.rounds_via("review", "implement"), 2);

        // And the third failure is the one over.
        report_outcome(&repo, &pipelines, "spinner", Outcome::Fail);
        let task = queued(&repo, "spinner");
        assert_eq!(
            task.stage(),
            "document",
            "`review`'s own exit, which with no `on_loop_max:` is its `on_pass`"
        );
        let log = task.section("## Status Log").unwrap_or_default();
        assert!(
            log.contains("`review` may not send this back to `implement` a 3rd time"),
            "{log}"
        );
    }

    /// A pass reported from `blocked` is the run clearing its own block, and
    /// it hands no budget back. A counter refunded every time the run stops
    /// itself is a counter that never runs out, which is the unbounded loop
    /// `loop:` exists to close — extending the licence is a person's call,
    /// and `resuming_hands_back_the_rounds_of_the_loop_it_resumes_into` is
    /// where that half is pinned.
    #[test]
    fn a_pass_reported_from_blocked_leaves_the_rounds_spent() {
        let repo = fixture("blocked-pass-keeps-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = Pipelines::builtin();
        let spent = review_limit(&pipelines);

        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.rounds.insert("review->implement".into(), spent);
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
            task.rounds_via("review", "implement"),
            spent,
            "the run cleared its own block, so the budget it spent stays spent"
        );
    }

    /// And the same for the other self-resume: an unattended run with nobody
    /// staffing `blocked` sends the task straight back to the step it stopped
    /// on, and that road hands nothing back either. Blocked below the limit
    /// here on purpose — a *spent* budget never reaches this road at all, by
    /// `apply_loop_budget`'s own carve-out — so what is under test is the
    /// refund, not the escalation.
    #[test]
    fn the_unattended_self_resume_leaves_the_rounds_spent() {
        let repo = unattended_fixture("unattended-resume-keeps-rounds");
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
        let repo = unattended_fixture("unattended-gate");
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
        let repo = fixture("attended-gate");
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
    /// passes back — with `work`'s budget for that failure and its exit both
    /// spelled out. The budget sits on `work` because `work` is what sends
    /// the task to `retry`; the counter it reads is still `work->retry`.
    fn looping_pipelines(limit: u32, on_loop_max: &str) -> Pipelines {
        let yaml = format!(
            "steps:\n  \
             - id: work\n    agent: pi\n    loop:\n      retry: {limit}\n    \
             on_loop_max: {on_loop_max}\n    on_pass: ship\n    on_fail: retry\n  \
             - id: retry\n    agent: pi\n    on_pass: work\n  \
             - id: ship\n    end: true\n"
        );
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", &yaml).unwrap(),
        );
        pipelines
    }

    /// The whole point of `on_loop_max`: a loop that will not converge can be
    /// told to carry on rather than stop. The findings ride along in the
    /// status log note, and the pull request is where a person reads them.
    #[test]
    fn a_spent_budget_takes_the_exit_the_step_named() {
        let repo = fixture("on-max-routes");
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(1, "ship");

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.rounds.insert("work->retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "ship");
        assert!(
            task.section("## Status Log").unwrap_or_default().contains(
                "`work` may not send this back to `retry` a 2nd time — carrying on \
                           to `ship`"
            ),
            "the move it refused, and what carried it on, have to travel with it"
        );
    }

    /// A default budget — no `on_loop_max:` at all — carries the task on to
    /// the step's own `on_pass`, exactly as an ordinary pass would.
    #[test]
    fn a_default_spent_budget_carries_on_to_on_pass() {
        let repo = fixture("default-loop-max");
        add(&repo, "stuck", &[]);
        // `work`'s own `on_pass` is `ship` — the same destination the other
        // `looping_pipelines` tests name explicitly with `on_loop_max:` — so
        // leaving it out here checks that the default really does take it.
        let yaml = "steps:\n  \
             - id: work\n    agent: pi\n    loop:\n      retry: 1\n    on_pass: ship\n    \
             on_fail: retry\n  \
             - id: retry\n    agent: pi\n    on_pass: work\n  \
             - id: ship\n    end: true\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.rounds.insert("work->retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        assert_eq!(
            queued(&repo, "stuck").stage(),
            "ship",
            "`work`'s own `on_pass` is `ship`, and that is where the default carries on to"
        );
    }

    /// A resumed lane is still a prompt, but sending the task back is what a
    /// lap costs now — so a loop on a `session:` step is bounded by how many
    /// times it sends work back, not by how many of the lanes that did so
    /// opened a conversation.
    #[test]
    fn backward_moves_not_conversations_spend_the_budget() {
        let repo = fixture("warm-loop");
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(2, "blocked");

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        // Six lanes on the `work->retry` route, but only one move down it.
        task.front.steps.insert("work->retry".into(), 6);
        task.front.rounds.insert("work->retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        assert_eq!(
            queued(&repo, "stuck").stage(),
            "retry",
            "six retried prompts on the same lap must not spend a budget of two laps"
        );
    }

    /// An `on_loop_max` naming a real step needs nobody, so an unattended run
    /// takes it exactly as an attended one does. This is what separates it
    /// from an exit resolving to `blocked`, which asks for a person and is
    /// skipped below.
    #[test]
    fn an_unattended_run_still_takes_an_exit_that_needs_nobody() {
        let repo = unattended_fixture("unattended-on-max-routes");
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(1, "ship");

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.rounds.insert("work->retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        assert_eq!(
            queued(&repo, "stuck").stage(),
            "ship",
            "the pipeline named where this loop goes, and getting there needs no person"
        );
    }

    /// And the other half of that: an `on_loop_max` that resolves to `blocked`
    /// is a request for a person, so unattended the budget is skipped outright
    /// rather than spent against a wall the run's own resume can only walk
    /// back into — it hands nothing back now.
    #[test]
    fn an_unattended_run_skips_a_budget_whose_exit_is_a_person() {
        let repo = unattended_fixture("unattended-on-max-blocked");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);
        let pipelines = looping_pipelines(1, "blocked");

        let mut task = queued(&repo, "stuck");
        task.set_stage("work", None);
        task.front.rounds.insert("work->retry".into(), 1);
        task.save().unwrap();

        report_outcome(&repo, &pipelines, "stuck", Outcome::Fail);

        let task = queued(&repo, "stuck");
        assert_eq!(
            task.stage(),
            "retry",
            "the loop goes round rather than parking"
        );
        assert_eq!(
            task.rounds_via("work", "retry"),
            2,
            "the bound is skipped outright, not spent and refunded, so this move still \
             banks a round exactly as an unbounded one would"
        );
    }

    /// A pipeline with one gated step, and a plain one after it to be let
    /// through to. Nothing shipped gates, so a gate test builds its own.
    fn gate_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
             - id: build\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    on_pass: deploy\n    \
               loop:\n      deploy: 2\n    on_loop_max: announce\n  \
             - id: deploy\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    gate: true\n    on_pass: announce\n    \
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
        clear_lane_env();
        let repo = fixture("gate-jumper");
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
        clear_lane_env();
        let repo = fixture("blocked-lane-resumes-sibling");
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

    /// Bounded even from `blocked`: a task waiting on a gate is a person's to
    /// answer, and `--stage` is not a lane's to hand it either.
    #[test]
    fn a_lane_on_blocked_may_not_resume_past_a_gate_or_reroute() {
        clear_lane_env();
        let repo = fixture("blocked-lane-cannot-cross-a-gate");
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

        add(&repo, "sibling", &[]);
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
        clear_lane_env();
        let repo = fixture("gate-release");
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

    /// `caught_at` — read by `resume` and the board alike — must still tell a
    /// gate catch from a pause raised at `blocked` itself for a task that
    /// paused before `paused_by` existed: `report` strips it here rather than
    /// setting it, the same shape a task file written before this key would
    /// carry. The fallback is `last_report.step == gated`, exactly what
    /// `caught_at` always inferred this from.
    #[test]
    fn a_gated_pass_with_no_paused_by_still_resumes_by_the_old_inference() {
        clear_lane_env();
        let repo = fixture("gate-release-no-paused-by");
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
        let repo = fixture("caught-at-paused-by-alone");
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.front.paused_at = Some("deploy".into());
        task.front.paused_by = Some("schedule".into());
        task.front.last_report = Some(crate::task::LastReport {
            step: "build".into(),
            outcome: "pass".into(),
            at: 0,
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
    /// whoever wrote its document, holds it after a step the pipeline never
    /// declared `gate: true` on — `build` here, which routes straight to
    /// `deploy` for every other task on this pipeline.
    #[test]
    fn a_tasks_own_gate_at_pauses_a_step_the_pipeline_never_gated() {
        clear_lane_env();
        let repo = fixture("gate-at");
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
        clear_lane_env();
        let repo = fixture("gate-at-fail");
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
        clear_lane_env();
        let repo = fixture("gate-at-block");
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
        clear_lane_env();
        let repo = fixture("gate-at-stale-blocked-from");
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
        clear_lane_env();
        let repo = fixture("gate-at-loop-max");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // The shipped `default` pipeline, minus its `blocked` step, with
        // `review`'s loop exit pointed at `blocked` by hand — the same shape
        // `a_budget_bound_for_a_person_does_not_bind_an_unattended_run_with_no_staffed_blocked_step`
        // builds, for an attended run instead.
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
            let review = pipeline
                .steps
                .iter_mut()
                .find(|s| s.id == "review")
                .unwrap();
            review.on_loop_max = Some(crate::pipeline::BLOCKED.to_string());
        }

        let spent = review_limit(&pipelines);
        let mut task = queued(&repo, "stuck");
        task.front.gate_at = Some("review".into());
        task.set_stage("review", None);
        task.front.rounds.insert("review->implement".into(), spent);
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
        clear_lane_env();
        let repo = unattended_fixture("gate-unattended");
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
        let repo = fixture("gate-unblock");
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
        let repo = fixture("gate-unblock");
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

    /// A loop that ran out of rounds is the one case where resuming at the step
    /// it stopped on is not enough on its own: the counter that stopped it is
    /// still spent, so the next transition walks into the same wall. Resuming
    /// gives that budget back — and only that one.
    #[test]
    fn resuming_hands_back_the_rounds_of_the_loop_it_resumes_into() {
        let repo = fixture("unblock-returns-rounds");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        add(&repo, "stuck", &[]);

        // Spent right up to `review`'s limit on the route back to `implement`, and
        // one round into a loop at the other end of the pipeline that nobody
        // has looked at.
        let mut task = queued(&repo, "stuck");
        task.set_stage("review", None);
        task.front.rounds.insert(
            "review->implement".into(),
            review_limit(&Pipelines::builtin()),
        );
        task.front.rounds.insert("awaiting->refresh".into(), 1);
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

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "review");
        assert_eq!(
            task.rounds_via("review", "implement"),
            0,
            "the loop it is resuming into must have its budget back, or the \
             next failure blocks it again"
        );
        assert_eq!(
            task.rounds_via("awaiting", "refresh"),
            1,
            "a loop nobody looked at keeps what it has spent"
        );
    }

    /// The lane that blocked did the reading, the exploring and usually the
    /// work; a fresh session on the same step pays for all of it again to get
    /// back where that one already was. Resuming marks the step to be
    /// continued instead, and names the step so that a launch of any *other*
    /// one cannot pick the mark up by accident.
    #[test]
    fn resuming_marks_the_step_it_resumes_to_be_continued() {
        let repo = fixture("unblock-marks-resume");
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
    #[test]
    fn resuming_a_task_parked_off_queued_lands_it_back_on_queued() {
        let repo = fixture("unpark-from-queued");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        crate::status::park(&mut task, "paused from the board", false);
        task.save().unwrap();
        assert_eq!(
            task.front.parked_from, None,
            "what `park` leaves on `queued`"
        );

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
    }

    /// A bare `spoolway resume` on a `p`-parked task reaches `unpark`, not
    /// `resume_at`: the round trip banks no lap, gives nothing back (there
    /// was nothing to give back), and marks the step to be continued the
    /// same way a real resume does.
    #[test]
    fn resuming_a_parked_task_puts_it_back_without_banking_a_lap() {
        let repo = fixture("unpark-round-trip");
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        let rounds_before = task.front.rounds.clone();
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
        let repo = fixture("unpark-stage-reroute");
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
             back would — left set, `start_one` would read the step's next \
             ordinary retry as a continued park"
        );
    }

    /// The same race a real resume closes for its own lane, closed for a
    /// park too: a stale settled lane left on the step a park just put the
    /// task back on would otherwise read as a lane still waiting for an
    /// answer, and no fresh one would ever start.
    #[test]
    fn unparking_frees_the_stale_lane_it_left_behind() {
        let mut repo = fixture("unpark-stale-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        add(&repo, "stuck", &[]);

        let mut task = queued(&repo, "stuck");
        task.set_stage("implement", None);
        task.front.parked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::PAUSED, "paused from the board");
        task.save().unwrap();

        let headless =
            crate::headless::Headless::new(&repo.root, &repo.config.dispatch, repo.headless_dir())
                .unwrap();
        headless
            .start_lane(&crate::mux::LaneSpec {
                name: &crate::mux::lane_name("implement", "stuck"),
                label: "implementer",
                kind: "pi",
                pane_id: "p1",
                args: &[],
                env: &std::collections::BTreeMap::new(),
                path_prefix: None,
            })
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

    /// A person who names a stage has rerouted the task rather than resumed it.
    /// The step they chose may have run hours and several steps ago, and
    /// reopening that conversation to answer a decision it never heard is not
    /// what they asked for — that one starts fresh.
    #[test]
    fn resuming_to_a_named_stage_starts_that_step_fresh() {
        let repo = fixture("unblock-elsewhere");
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

    /// What the shipped `review` step allows on the route back to `implement`,
    /// read rather than written down twice.
    fn review_limit(pipelines: &Pipelines) -> u32 {
        pipelines
            .get("default")
            .unwrap()
            .step("review")
            .unwrap()
            .round_limit("implement")
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
        let mut repo = fixture("unblock-stale-lane");
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
        let headless =
            crate::headless::Headless::new(&repo.root, &repo.config.dispatch, repo.headless_dir())
                .unwrap();
        headless
            .start_lane(&crate::mux::LaneSpec {
                name: "stuck · handover",
                label: "github",
                kind: "pi",
                pane_id: "p1",
                args: &[],
                env: &std::collections::BTreeMap::new(),
                path_prefix: None,
            })
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
}
