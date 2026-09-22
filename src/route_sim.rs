//! A walk over [`crate::commands::route`], the pure routing decision
//! `spoolway report` now goes through — proving dynamically what
//! [`crate::pipeline::Pipeline::check_bounded_loops`] only proves on paper.
//!
//! `check_bounded_loops` shows that a pipeline's own graph always has a way
//! out. It does not touch the counters that actually gate a loop at
//! runtime — `rounds`, banked one lap at a time in [`crate::task::Task::
//! set_stage`] — because `blocked` is not a step any pipeline file declares:
//! it is read from task state and the reported outcome, in `route` itself.
//! So the graph the checker reads and the graph the dispatcher walks are two
//! different things, and a bug could make them disagree. This module walks
//! the second one directly: every outcome, at every step, from a task with a
//! clean slate, over the pipelines this project ships and over pipeline
//! shapes generated to the same rules `spoolway pipeline check` holds a
//! project's own files to (`Pipeline::parse` runs `Pipeline::validate()`
//! before handing a shape back, exactly as `pipeline_check`'s own
//! `pipelines.validate()` does for the structural half of that check — the
//! half this module can run with no `Repo` and no agent config to consult).
//!
//! Everything here runs in memory: a [`crate::task::Task`] built from a
//! literal frontmatter, never saved to disk, and a [`crate::pipeline::
//! Pipeline`] parsed from a literal or generated YAML string. The two
//! directories this project keeps its own pipelines under are read from
//! disk once, at the top of the one test below — that is not a rule `route`
//! itself is held to, only a convenient way for this module to find the
//! shapes it is asked to walk.

#[cfg(test)]
mod tests {
    use crate::commands::route;
    use crate::pipeline::{Outcome, Pipeline, Pipelines, StepKind};
    use crate::task::Task;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    /// A tiny, deterministic generator — `splitmix64` — so a failing walk can
    /// be reproduced from the one number it prints. Nothing here needs a real
    /// PRNG's statistical properties, only that the same seed always yields
    /// the same shape and that nearby seeds do not yield near-identical ones.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// A number below `n`, `n > 0`.
        fn below(&mut self, n: u32) -> u32 {
            (self.next_u64() % u64::from(n)) as u32
        }

        /// `true` about `pct` percent of the time.
        fn chance(&mut self, pct: u32) -> bool {
            self.below(100) < pct
        }
    }

    /// A pipeline shape built to already satisfy [`Pipeline::validate`]'s
    /// rules — a forward chain of 2 to 5 steps, with the occasional bounded
    /// back edge, gate, terminal step and declared `blocked` step folded in —
    /// rather than a purely random document filtered down to the rare one
    /// that parses. The rules that keep it valid by construction:
    ///
    /// * every `on_fail` back edge names a *strictly earlier* step, so the
    ///   cycle it can only ever close runs through the forward chain in
    ///   front of it;
    /// * every such edge carries a `loop:` bound keyed on exactly the step it
    ///   targets, so [`Pipeline::check_bounded_loops`] can find the budget
    ///   that breaks that cycle;
    /// * that budget's exit (`on_loop_max`, or `on_pass` when unset) always
    ///   lands outside the cycle it bounds — forward in the chain, or
    ///   `blocked` — so the escalation cannot be read back into the very
    ///   loop it was meant to leave.
    ///
    /// Still handed to [`Pipeline::parse`] rather than trusted outright: a
    /// shape this reasoning got wrong is simply skipped by the caller below,
    /// the same as one `spoolway pipeline check` would refuse for a reason
    /// this comment did not anticipate.
    fn generate(seed: u64) -> String {
        let mut rng = Rng::new(seed);
        let steps = 2 + rng.below(4); // 2..=5

        let mut out = String::from("steps:\n");
        for i in 0..steps {
            let id = format!("s{i}");
            let command = rng.chance(20);
            out += &format!("  - id: {id}\n");
            if command {
                out += "    run: \"true\"\n";
            } else {
                out += "    agent: claude\n";
                if rng.chance(15) {
                    out += "    gate: true\n";
                }
            }

            // The back edge, before `on_pass`: an earlier step to fail to,
            // bounded on that exact route, with a exit that lands outside
            // the cycle it closes — see the doc comment above.
            if i > 0 && rng.chance(60) {
                let target = rng.below(i);
                let limit = 1 + rng.below(3); // 1..=3
                out += &format!("    loop:\n      s{target}: {limit}\n");
                if rng.chance(50) {
                    out += "    on_loop_max: blocked\n";
                }
                out += &format!("    on_fail: s{target}\n");
            }

            let is_last = i + 1 == steps;
            if is_last && rng.chance(30) {
                out += "    on_pass: term\n";
            } else if is_last {
                out += "    on_pass: done\n";
            } else {
                out += &format!("    on_pass: s{}\n", i + 1);
            }
        }

        if out.contains("on_pass: term") {
            out += "  - id: term\n    end: true\n";
        }

        // Always declared, never left to a coin flip: every real project's
        // pipeline goes through `Pipelines::assemble` before anything routes
        // through it, which materialises `blocked` from `[unattended]`
        // unconditionally — so `blocked_is_staffed` is `unattended` in
        // practice for any pipeline this walk could actually meet in
        // production (see that method's own doc comment). A generated shape
        // that left `blocked` undeclared would be testing a configuration
        // — unattended with no lane to staff `blocked` at all — that
        // `apply_loop_budget` already documents as unbounded on purpose,
        // held only by `unattended.max_output_tokens`/`max_cost_usd`
        // outside routing entirely; report.rs's own
        // `an_unattended_run_skips_a_budget_whose_exit_is_a_person` proves
        // exactly that carve-out already. Declaring it here keeps this walk
        // inside the shapes routing is actually meant to bound.
        out += "  - id: blocked\n    agent: claude\n";

        out
    }

    /// A task on `stage`, with nothing else recorded — the clean slate every
    /// walked path other than `blocked` itself starts from. In memory only:
    /// [`Task::parse`] never touches disk, so nothing here needs the scratch
    /// directory the rest of this crate's fixtures write into.
    ///
    /// Never used to start a walk *at* `blocked` — see
    /// [`fresh_task_blocked_from`] for why that needs a task this blank
    /// cannot stand in for.
    fn fresh_task(stage: &str) -> Task {
        Task::parse(
            std::path::PathBuf::new(),
            &format!("---\nid: sim\nstage: {stage}\n---\n"),
        )
        .expect("a generated stage name is always a valid task id fragment")
    }

    /// A task already sitting on `blocked`, having stopped there from
    /// `origin` — the shape every real blocked task actually has.
    /// [`resume_target`]'s own doc names the fact this exists to satisfy:
    /// "nothing else leaves all four [`blocked_from`, `last_report`,
    /// `worktree_path`, `workspace_id`] unset — a launch that failed records
    /// `blocked_from`, a lane that ran records a checkout." A task built by
    /// [`fresh_task`] instead leaves all four unset, which is the one shape
    /// `resume_target` reads as "never started" and answers with `queued` —
    /// a real destination, but not one any task actually on `blocked` can
    /// reach, and not a step this walk can run further outcomes through. A
    /// walk seeded that way never exercises `cleared_block_target`'s
    /// agent/command split or the loop-budget hand-back in `resume_at` at
    /// all: every `blocked` start looked identical and rested one hop in
    /// regardless of what it should have done.
    fn fresh_task_blocked_from(origin: &str) -> Task {
        Task::parse(
            std::path::PathBuf::new(),
            &format!(
                "---\nid: sim\nstage: {blocked}\nblocked_from: {origin}\n---\n",
                blocked = crate::pipeline::BLOCKED,
            ),
        )
        .expect("a generated stage name is always a valid task id fragment")
    }

    /// Whether a task arriving at `stage` stops there for this walk's own
    /// purposes: `done`, `paused`, `blocked`, or any declared
    /// [`StepKind::Terminal`] step — exactly the reserved and declared
    /// resting states this project's own routing recognises, and nothing
    /// wider. A stage that is none of these and not a step this pipeline
    /// declares either is not "resting" — see [`walk`]'s own check for what
    /// happens to one of those instead.
    ///
    /// `blocked` rests even when [`Pipeline::blocked_is_staffed`] says an
    /// unattended run keeps a lane on it. That is not a shortcut around
    /// property 1 — it is the same fact [`Pipeline::destinations`]'s own doc
    /// comment already states about the static graph: "its real routing...
    /// is decided at runtime from task state... not from the graph.
    /// Reporting a self-edge here would read as the very unbounded cycle
    /// `blocked` is deliberately exempt from." A command step hands a
    /// cleared block straight back to itself (see `cleared_block_target`),
    /// so an unblocker session that keeps saying pass and a command that
    /// keeps failing can bounce between the two forever — by design, held
    /// only by `unattended.max_output_tokens`/`max_cost_usd`, a brake this
    /// walk has no `Repo` to read and `route` never touches. What this walk
    /// still proves at that boundary is [`walk_from_blocked`]'s own job: a
    /// task that actually stopped at some real step, seeded with that
    /// step's own `blocked_from`, is routed and checked for the rounds
    /// properties on every outcome — including the one hop a `--pass` takes
    /// back into real work, which this walk then carries on from exactly as
    /// it would any other arrival — without this walk also having to prove
    /// that a lane bouncing off `blocked` forever eventually gives up, which
    /// routing alone was never the thing bounding.
    fn is_resting(pipeline: &Pipeline, stage: &str) -> bool {
        if stage == crate::pipeline::DONE
            || stage == crate::pipeline::PAUSED
            || stage == crate::pipeline::BLOCKED
        {
            return true;
        }
        pipeline
            .step(stage)
            .is_some_and(|step| step.kind() == StepKind::Terminal)
    }

    /// Property 2 and 3 together: a `rounds` map that only ever grows. Route
    /// never removes an entry on its own — only a person's own
    /// `spoolway resume` does that, through `resume_at`'s `by_hand: true`
    /// arm, which nothing on this walk's road ever passes — so the one
    /// shape a bug could produce here is a route that failed to bank a lap
    /// it should have, or banked the wrong one. Either reads as a count that
    /// should have risen and did not, exactly what this compares for.
    fn rounds_only_rise(before: &BTreeMap<String, u32>, after: &BTreeMap<String, u32>) -> bool {
        before
            .iter()
            .all(|(route, count)| after.get(route).copied().unwrap_or(0) >= *count)
    }

    /// Everything about a task that can change what [`route`] does with it
    /// next, once it has left `blocked` behind. `blocked_from` is not part
    /// of this: it is read only when `current` is `blocked` itself, and
    /// [`is_resting`] never lets this walk recurse back onto `blocked` once
    /// it has left, so a stale `blocked_from` never reaches another `route`
    /// call — `resume_at` clears it on the very road that leaves `blocked`
    /// in the first place. `gate_at`, `paused_at` and `resume` are not part
    /// of it either, but not all three for the same reason, so take them
    /// separately. `paused_at` and `resume` are written and never read back:
    /// `route` sets `paused_at` on both of its paused roads and, through
    /// `resume_at`, sets `resume` on more than one road now, `walk_from_blocked`
    /// included — but nothing `route` calls branches on either, so whatever
    /// they hold cannot change the branch a task takes. `gate_at` is the other
    /// way round: `route` *does* read it, through `gate_hold`, on every single
    /// hop, and a task carrying one would be parked on `paused` — it stays out
    /// of the key only because nothing on this walk ever sets it. No fixture
    /// writes it, and the one write `route` has is a *clear* — it drops a
    /// fired `gate_at` back to `None` — so it is `None` on every task this
    /// walk routes, and that clear is itself unreachable here. That second
    /// reason is the weaker of the two, since a fixture could take it away
    /// without touching this file, so [`step_once`] asserts it on every hop
    /// rather than leaving it to this comment. The test to apply before
    /// adding a field to a fixture is whichever of the two shapes above the
    /// field answers to: a field `route` reads must either be in the key or
    /// be held constant across the whole walk, and held by an assertion, not
    /// by prose. The step a task sits on plus its `rounds` is the whole of
    /// what a repeat visit needs to be recognised by.
    type State = (String, BTreeMap<String, u32>);

    /// The three facts that never change over one walk — bundled so `walk`
    /// takes a state (`task`, `current`, `depth`, `path`, `seen`) and a
    /// context (this) rather than eight loose arguments.
    struct Walk<'a> {
        pipeline: &'a Pipeline,
        unattended: bool,
        /// Lane bound — property 4's own "stated bound". See [`lane_bound`].
        cap: usize,
        /// The bounded route counters that can still affect routing from each
        /// step. Historical counters behind a one-way phase boundary cannot
        /// change a later decision, so keeping them in [`State`] would split
        /// one routing state into many identical copies.
        relevant_rounds: HashMap<String, Vec<String>>,
    }

    /// Build the part of `rounds` that can still affect a future routing
    /// decision from each step. `route` reads only counters for bounded moves,
    /// and only sources reachable before a resting state can run again.
    fn relevant_rounds(pipeline: &Pipeline) -> HashMap<String, Vec<String>> {
        let mut by_step = HashMap::new();

        for origin in pipeline.step_ids() {
            let mut reachable = HashSet::new();
            let mut pending = vec![origin];
            let mut keys = BTreeSet::new();

            while let Some(current) = pending.pop() {
                if !reachable.insert(current) {
                    continue;
                }
                let Some(step) = pipeline.step(current) else {
                    continue;
                };

                for destination in pipeline.destinations(step) {
                    if step.round_limit(destination).is_some() {
                        keys.insert(crate::task::route_key(current, destination));
                    }
                    if !is_resting(pipeline, destination) {
                        pending.push(destination);
                    }
                }
            }

            by_step.insert(origin.to_string(), keys.into_iter().collect());
        }

        by_step
    }

    fn state_key(ctx: &Walk<'_>, task: &Task, current: &str) -> State {
        let rounds = ctx
            .relevant_rounds
            .get(current)
            .into_iter()
            .flatten()
            .filter_map(|key| {
                task.front
                    .rounds
                    .get(key)
                    .copied()
                    .map(|count| (key.clone(), count))
            })
            .collect();
        (current.to_string(), rounds)
    }

    /// Walk every outcome at every step reachable from `current`. `Err`
    /// names the property that broke and the path that broke it; the
    /// caller adds the seed.
    ///
    /// A plain tree walk over every outcome at every step is exponential in
    /// the number of independent loops a pipeline declares — this project's
    /// own `bugfix.yml` alone bounds four separate routes, and an
    /// adversarial path can spend all four before any one of them forces an
    /// exit. What actually matters is never the *path*, only the *state* a
    /// path is in — `current` plus `rounds`, see [`State`] — so `seen` memoises
    /// it: `Explored` skips a state this walk has already proven safe by
    /// some other route in, and `OnStack` catches a state reappearing while
    /// it is still being proven, which is a real, unbounded cycle rather
    /// than this walk simply taking too long to notice one. Between them
    /// this turns what would be a search over paths (exponential) into a
    /// search over states (bounded by how many distinct `rounds` snapshots a
    /// pipeline's own `loop:` limits allow, which is exactly the small
    /// number `check_bounded_loops` reasons about on paper).
    fn walk(
        ctx: &Walk,
        task: &Task,
        current: &str,
        depth: usize,
        path: &mut Vec<String>,
        seen: &mut HashMap<State, Seen>,
    ) -> Result<(), String> {
        let state = state_key(ctx, task, current);
        match seen.get(&state) {
            Some(Seen::Explored) => return Ok(()),
            Some(Seen::OnStack) => {
                return Err(format!(
                    "`{current}` (rounds {:?}) recurs while still being proven bounded — a real \
                     cycle, not just a slow walk: {}",
                    task.front.rounds,
                    path.join(" | ")
                ));
            }
            None => {}
        }
        seen.insert(state.clone(), Seen::OnStack);

        for &outcome in outcomes_at(current) {
            let (branch, destination) = step_once(ctx, task, current, outcome, path)?;

            if is_resting(ctx.pipeline, &destination) {
                path.pop();
                continue;
            }

            if depth + 1 >= ctx.cap {
                return Err(format!(
                    "still running after {} lane(s), never reaching a terminal step: {}",
                    ctx.cap,
                    path.join(" | ")
                ));
            }

            walk(ctx, &branch, &destination, depth + 1, path, seen)?;
            path.pop();
        }

        seen.insert(state, Seen::Explored);
        Ok(())
    }

    /// Every outcome that actually reaches a step's `destination` —
    /// `--pause` is only ever refused everywhere but `blocked` at
    /// `report.rs`'s own CLI boundary, before `route` is ever called, so
    /// walking it anywhere else would be proving a property about an input
    /// nothing can produce.
    fn outcomes_at(current: &str) -> &'static [Outcome] {
        if current == crate::pipeline::BLOCKED {
            &[Outcome::Pass, Outcome::Fail, Outcome::Block, Outcome::Pause]
        } else {
            &[Outcome::Pass, Outcome::Fail, Outcome::Block]
        }
    }

    /// Route one outcome from `current`, bank it the way [`crate::commands::
    /// report`] itself does right after calling [`route`], and check
    /// properties 2 and 3 immediately, plus the one fact every destination
    /// has to satisfy before either caller can do anything with it: it is
    /// either [`is_resting`] or a step this pipeline actually declares.
    /// This is the one place every entry point into this walk (a plain
    /// step, or `blocked` seeded with a candidate `blocked_from`) shares, so
    /// the checks and their error messages stay written once. `path` is
    /// appended to on the way out, on both the `Ok` and the `Err` road, so a
    /// caller that goes on to fail a later check still has this hop in the
    /// path it prints.
    fn step_once(
        ctx: &Walk,
        task: &Task,
        current: &str,
        outcome: Outcome,
        path: &mut Vec<String>,
    ) -> Result<(Task, String), String> {
        let mut branch = task.clone();
        // The half of `State`'s soundness that no fixture is stopped from
        // breaking: `route` reads `gate_at` through `gate_hold` on every hop,
        // and it is left out of the memo key only because it is `None` on
        // every task this walk ever routes. Checked here rather than trusted,
        // so a fixture that starts setting it fails this walk instead of
        // quietly making its memo — and so the whole proof — unsound.
        assert!(
            branch.front.gate_at.is_none(),
            "a fixture set `gate_at` ({:?}) — `route` branches on it, so it \
             belongs in `State`'s memo key; see that type's doc",
            branch.front.gate_at
        );
        let before = branch.front.rounds.clone();
        let routed = route(
            &mut branch,
            ctx.pipeline,
            current,
            outcome,
            ctx.unattended,
            None,
        )
        .map_err(|e| format!("route() itself refused `{current}` --{outcome}-->: {e:#}"))?;
        branch.set_stage(&routed.destination, None);
        path.push(format!("{current} --{outcome}--> {}", routed.destination));

        if !rounds_only_rise(&before, &branch.front.rounds) {
            return Err(format!(
                "`rounds` lost a lap on the last hop of {}: {before:?} -> {:?}",
                path.join(" | "),
                branch.front.rounds
            ));
        }

        let destination = routed.destination;
        if !is_resting(ctx.pipeline, &destination) && ctx.pipeline.step(&destination).is_none() {
            // Neither a resting stage nor a step this pipeline declares —
            // `route` (or this walk's own fixtures) sent the task somewhere
            // nothing can run it further from and nothing recognises as a
            // stop, which is exactly the shape of bug a blank
            // `blocked_from` produced before `walk_from_blocked` existed:
            // `resume_target`'s "never started" fallback to `queued`.
            return Err(format!(
                "the last hop of {} lands on `{destination}`, neither a resting stage nor a \
                 step this pipeline declares",
                path.join(" | ")
            ));
        }

        Ok((branch, destination))
    }

    /// What [`walk`] knows about a `State` it has already visited on this
    /// (pipeline, unattended) run.
    #[derive(Clone, Copy)]
    enum Seen {
        /// Still on the call stack that is proving it bounded — a repeat
        /// visit while this is still the answer is a genuine cycle.
        OnStack,
        /// Proven to reach a resting step within the bound, once. Nothing
        /// about revisiting it a second time would prove anything new.
        Explored,
    }

    /// Every outcome `blocked` can be reported with, once for each step in
    /// the pipeline it could plausibly have stopped at — the missing half
    /// [`walk`] itself cannot exercise, because [`is_resting`] never lets
    /// its own recursion continue past `blocked` (see that function's own
    /// doc for why), and because `blocked`'s routing genuinely depends on
    /// which step a task stopped at, which a bare `(current, rounds)`
    /// [`State`] cannot tell apart.
    ///
    /// Each origin gets its own [`fresh_task_blocked_from`] and its own
    /// `path`, walked independently of the others — `blocked` itself is
    /// deliberately never entered into `seen`, since two different origins
    /// disagree about what happens there and a memo keyed only on `(blocked,
    /// {})` would let the first one answer for all of them. Once a `--pass`
    /// carries the task off `blocked` into a real step, though, that step's
    /// own state no longer depends on which origin got it there — see
    /// [`State`]'s own doc — so from that hop on this shares `seen` with
    /// every other walk over this pipeline, the same as [`walk`] does with
    /// itself.
    fn walk_from_blocked(ctx: &Walk, seen: &mut HashMap<State, Seen>) -> Result<(), String> {
        for origin in ctx.pipeline.step_ids() {
            if origin == crate::pipeline::BLOCKED {
                continue;
            }
            let task = fresh_task_blocked_from(origin);

            for &outcome in outcomes_at(crate::pipeline::BLOCKED) {
                let mut path = Vec::new();
                let (branch, destination) =
                    step_once(ctx, &task, crate::pipeline::BLOCKED, outcome, &mut path)
                        .map_err(|e| format!("blocked_from `{origin}`: {e}"))?;

                if is_resting(ctx.pipeline, &destination) {
                    continue;
                }

                walk(ctx, &branch, &destination, 1, &mut path, seen)
                    .map_err(|e| format!("blocked_from `{origin}`: {e}"))?;
            }
        }
        Ok(())
    }

    /// The lane-count bound one pipeline's walked paths are held to —
    /// property 4's own "stated bound", and [`walk`]'s own `cap`.
    ///
    /// Computed per pipeline rather than one constant for every shape: two
    /// or more of this project's own loops can compound (`bugfix.yml`'s
    /// `review → fix`, `reproduce-again → fix`, `test → reproduce-again` and
    /// `suite → reproduce-again` all bound different routes, so an
    /// adversarial walk can spend every one of them along a single path
    /// before any of them forces an exit), and a single small constant
    /// picked against a two-step fixture undercounted a real shipped
    /// pipeline the first time this ran. Summing every bounded route's own
    /// limit and tripling it, plus three lanes a step for the unbounded
    /// forward chain between them, is generous enough that a path reaching
    /// it is a real bug rather than a bound too tight for its own pipeline.
    fn lane_bound(pipeline: &Pipeline) -> usize {
        let bounded: u32 = pipeline
            .steps
            .iter()
            .map(|step| match &step.r#loop {
                crate::pipeline::Loop::Every(n) => *n,
                crate::pipeline::Loop::PerRoute(by_route) => by_route.values().sum(),
            })
            .sum();
        (bounded as usize) * 3 + pipeline.steps.len() * 3 + 12
    }

    /// How many generated shapes this walk holds itself to, on top of every
    /// pipeline this project ships. Large enough to be worth calling a
    /// simulation; the actual path count per shape is the thing the time
    /// budget below is watching.
    const GENERATED_SHAPES: usize = 150;

    /// Every outcome at every step, over every pipeline this project ships
    /// and a few hundred generated shapes, asserting the four properties a
    /// routing bug breaks: every path reaches a terminal step within
    /// [`lane_bound`]'s own count for that pipeline, no bounded `rounds`
    /// entry is ever removed, every route's count only rises, and the walk
    /// itself finishes in time to run on every `cargo test`.
    #[test]
    fn every_outcome_at_every_step_reaches_a_bounded_terminal() {
        let start = std::time::Instant::now();
        let base_seed: u64 = 0xC0FF_EE00_D15E_A5E5;

        let mut suites: Vec<(String, Pipeline)> = Vec::new();

        let shipped = Pipelines::shipped(&crate::config::Config::default())
            .expect("assets/pipelines/*.yml must parse and validate — pipeline_check's own bar");
        for (name, pipeline) in shipped.pipelines {
            suites.push((format!("assets/pipelines/{name}.yml"), pipeline));
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dev = Pipelines::load_tracked(root, &crate::config::Config::default())
            .expect(".spoolway/pipelines/*.yml must parse and validate — pipeline_check's own bar");
        for (name, pipeline) in dev.pipelines {
            suites.push((format!(".spoolway/pipelines/{name}.yml"), pipeline));
        }

        let mut seed = base_seed;
        let mut generated = 0;
        let mut attempts = 0;
        while generated < GENERATED_SHAPES && attempts < GENERATED_SHAPES * 20 {
            seed = Rng::new(seed).next_u64();
            attempts += 1;
            let yaml = generate(seed);
            // Only a shape `spoolway pipeline check` would accept is worth
            // walking — `Pipeline::parse` runs `Pipeline::validate()` before
            // handing one back, which is the structural half of that check.
            // A shape this generator's own reasoning got wrong (see
            // `generate`'s doc comment) is skipped here exactly as it would
            // be refused there.
            if let Ok(pipeline) = Pipeline::parse(&format!("generated-{seed:016x}"), &yaml) {
                suites.push((format!("generated (seed {seed:#018x})"), pipeline));
                generated += 1;
            }
        }
        assert!(
            generated >= GENERATED_SHAPES / 2,
            "only {generated}/{GENERATED_SHAPES} generated shapes parsed from seed \
             {base_seed:#018x} after {attempts} attempts — the generator in this file no \
             longer matches `Pipeline::validate`'s rules"
        );

        for (label, pipeline) in &suites {
            let cap = lane_bound(pipeline);
            for unattended in [false, true] {
                let ctx = Walk {
                    pipeline,
                    unattended,
                    cap,
                    relevant_rounds: relevant_rounds(pipeline),
                };
                // Shared across every starting step: a state this walk has
                // already proven safe from one start is exactly as safe
                // reached from another, and most states are — see `walk`'s
                // own doc for why this is what keeps the whole thing inside
                // the five-second budget.
                let mut seen: HashMap<State, Seen> = HashMap::new();
                for step in pipeline.step_ids() {
                    // `blocked` needs a candidate `blocked_from` to route
                    // correctly at all — see `fresh_task_blocked_from`'s own
                    // doc — so it is walked by `walk_from_blocked`, once per
                    // origin, rather than as one more plain starting step.
                    let result = if step == crate::pipeline::BLOCKED {
                        walk_from_blocked(&ctx, &mut seen)
                    } else {
                        let task = fresh_task(step);
                        let mut path = Vec::new();
                        walk(&ctx, &task, step, 0, &mut path, &mut seen)
                    };
                    if let Err(detail) = result {
                        panic!(
                            "{label} (unattended: {unattended}), starting from `{step}`, seed \
                             {base_seed:#018x}, bound {cap}: {detail}"
                        );
                    }
                }
            }
        }

        // A blowup guard, not a benchmark. The walk is linear in shapes x
        // paths, so a routing change that makes it super-linear shows up as
        // multiples rather than as a few percent — and the budget has to
        // clear the slowest machine that runs it, not the fastest. Measured:
        // ~2.0s on a developer box against 5.0-5.6s on this project's CI
        // runner, which is why a 5s budget failed on every branch of a
        // five-deep stack while passing locally every time. Twenty seconds is
        // about four times the slowest run yet observed and still leaves this
        // a `cargo test`-sized proof.
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 20,
            "the walk took {elapsed:?} — either shrink {GENERATED_SHAPES} generated shapes or \
             `lane_bound`'s own multiplier, or this has stopped being a `cargo test`-sized proof"
        );
    }
}
