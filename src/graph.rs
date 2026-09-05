//! The task dependency graph: what `depends_on` means once you look at all the
//! task files at once rather than one at a time.
//!
//! The edges themselves live in the task files (`depends_on:`), and a task's
//! `queued` step is the only place they are enforced — a task starts when every
//! task it names has finished. This module is what turns that edge set into a
//! graph: it answers whether a task is ready, what it is still waiting on,
//! whether it is waiting on something that can never finish, and how much other
//! work finishing it would release.
//!
//! Nothing here touches git. A dependent's worktree is cut straight from its
//! first dependency's branch — see `ensure_workspace` in `src/dispatch.rs` —
//! rather than from the group's base, so ancestry is a fact of the cut
//! itself, settled the instant the worktree is made, not something a later
//! rebase has to go build.
//!
//! The graph is rebuilt from scratch on every pass and owns its data, which is
//! what lets a pass keep mutating task files while holding one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::{Result, bail};

use crate::pipeline::{Pipelines, StepKind};
use crate::task::Task;

/// Where a task stands, seen from something waiting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepState {
    /// Finished. Its changes are on the group's base branch, so a dependent cut from
    /// that branch will contain them.
    Done,
    /// Sitting on the pipeline's `blocked` step, parked for a person. Terminal,
    /// so it will never advance on its own: everything downstream is stranded
    /// until a person runs `spoolway resume`.
    ///
    /// Not what an unattended run's own block reaches, on a pipeline that does
    /// not staff `blocked` — that road resumes the lane that hit it instead of
    /// parking, so the task never actually sits here. And not what a *staffed*
    /// `blocked` step reaches either, whatever the run: a lane is running
    /// there, or about to be, so a dependent is still moving and waits
    /// quietly rather than being reported as stranded. See
    /// [`crate::pipeline::Pipeline::blocked_is_staffed`].
    Dead,
    /// Still moving through the pipeline.
    Pending,
    /// Named by a `depends_on` but present in neither the queue nor the
    /// archive. Almost always a typo, and permanent if left alone.
    Unknown,
}

/// Every queued task's dependencies, resolved against each other.
pub struct Graph {
    state: BTreeMap<String, DepState>,
    /// Task -> what it declares in `depends_on`.
    edges: BTreeMap<String, Vec<String>>,
    /// Task -> tasks that name it. The reverse of `edges`.
    reverse: BTreeMap<String, Vec<String>>,
    /// Task -> unfinished tasks in its group, including itself.
    group_open: BTreeMap<String, usize>,
    /// Group names that have already produced work — see [`Graph::group_is_open`].
    open_groups: BTreeSet<String>,
    /// Task -> the longest chain of unfinished dependencies standing above it
    /// — see [`Graph::depth`].
    depth: BTreeMap<String, usize>,
    cycles: Vec<Vec<String>>,
}

impl Graph {
    /// Build against no run in particular: a `blocked` task always classifies
    /// as [`DepState::Dead`], which is right when there is no dispatcher to
    /// ask whether it staffs that step — `spoolway queue show`, tests, and any
    /// other caller with no run in progress. [`Graph::build_for_run`] is the
    /// one to use from inside a pass, where that answer actually depends on
    /// whether this run is unattended.
    pub fn build(tasks: &[Task], pipelines: &Pipelines, archive_dir: &Path) -> Graph {
        Graph::build_inner(tasks, pipelines, archive_dir, false)
    }

    /// Same as [`Graph::build`], but told whether this run is unattended —
    /// needed to classify a dependency sitting on a staffed `blocked` step as
    /// still moving rather than dead. See
    /// [`crate::pipeline::Pipeline::blocked_is_staffed`].
    pub fn build_for_run(
        tasks: &[Task],
        pipelines: &Pipelines,
        archive_dir: &Path,
        unattended: bool,
    ) -> Graph {
        Graph::build_inner(tasks, pipelines, archive_dir, unattended)
    }

    fn build_inner(
        tasks: &[Task],
        pipelines: &Pipelines,
        archive_dir: &Path,
        unattended: bool,
    ) -> Graph {
        let mut state: BTreeMap<String, DepState> = BTreeMap::new();
        let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for task in tasks {
            state.insert(task.front.id.clone(), classify(task, pipelines, unattended));
            edges.insert(task.front.id.clone(), task.front.depends_on.clone());
        }

        // A dependency naming no queued task either finished and was archived,
        // or does not exist at all. Only the second is a problem, and only the
        // filesystem can tell them apart.
        for dep in tasks.iter().flat_map(|t| &t.front.depends_on) {
            if state.contains_key(dep) {
                continue;
            }
            let archived = archive_dir.join(format!("{dep}.md")).exists();
            state.insert(
                dep.clone(),
                if archived {
                    DepState::Done
                } else {
                    DepState::Unknown
                },
            );
        }

        let mut reverse: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, deps) in &edges {
            for dep in deps {
                reverse.entry(dep.clone()).or_default().push(id.clone());
            }
        }

        // Unfinished tasks per group. Scheduling uses this to drain one group
        // rather than spread across several: a group only reaches main once
        // all of it is merged, so the one with least left to do is worth most.
        let mut per_group: BTreeMap<String, usize> = BTreeMap::new();
        for task in tasks {
            if state.get(task.id()) == Some(&DepState::Done) {
                continue;
            }
            if let Some(group) = &task.front.group {
                *per_group.entry(group.clone()).or_insert(0) += 1;
            }
        }
        let group_open = tasks
            .iter()
            .map(|task| {
                // A task belonging to no group is a group of one — already as
                // close to done as a group can be.
                let open = match &task.front.group {
                    Some(group) => {
                        // `per_group` above skipped every task already sitting
                        // on `done`, this one included, and this map counts the
                        // task itself. Adding it back matters on exactly one
                        // caller: `[issue_tracking]`'s `done` event, whose
                        // `SPOOLWAY_GROUP_LAST` asks whether anybody else is
                        // left. Without this every task reads as its group's
                        // last, because by the pass its own `done` hook fires
                        // it is no longer one of the group's unfinished tasks.
                        let others = per_group.get(group).copied().unwrap_or(0);
                        others + usize::from(state.get(task.id()) == Some(&DepState::Done))
                    }
                    None => 1,
                };
                (task.front.id.clone(), open.max(1))
            })
            .collect();

        // A group is open once it has produced work worth not scattering
        // effort away from: either a live task of it has moved past `queued`,
        // or — the case a chain's last task hits once every task before it
        // has been archived — one of its tasks names a `depends_on` that
        // finished and is not itself sitting in the queue. `edges` is keyed
        // by every live task's id, so "not in the queue" is exactly "not a
        // key of `edges`".
        let mut open_groups: BTreeSet<String> = BTreeSet::new();
        for task in tasks {
            let Some(group) = &task.front.group else {
                continue;
            };
            if open_groups.contains(group) {
                continue;
            }
            if task.stage() != crate::pipeline::QUEUED {
                open_groups.insert(group.clone());
                continue;
            }
            let opened_by_a_finished_dep = task
                .front
                .depends_on
                .iter()
                .any(|dep| state.get(dep) == Some(&DepState::Done) && !edges.contains_key(dep));
            if opened_by_a_finished_dep {
                open_groups.insert(group.clone());
            }
        }

        let cycles = find_cycles(&edges);
        let depth = compute_depths(&edges, &state, &cycles);

        Graph {
            state,
            edges,
            reverse,
            group_open,
            open_groups,
            depth,
            cycles,
        }
    }

    pub fn state(&self, id: &str) -> DepState {
        self.state.get(id).copied().unwrap_or(DepState::Unknown)
    }

    fn deps(&self, id: &str) -> impl Iterator<Item = &str> + use<'_> {
        self.edges.get(id).into_iter().flatten().map(String::as_str)
    }

    /// Every dependency has finished, so this task may start.
    pub fn ready(&self, id: &str) -> bool {
        self.deps(id).all(|dep| self.state(dep) == DepState::Done)
    }

    /// Dependencies that have not finished yet, in declaration order.
    pub fn waiting_on(&self, id: &str) -> Vec<&str> {
        self.deps(id)
            .filter(|dep| self.state(dep) != DepState::Done)
            .collect()
    }

    /// The nearest dependency, transitively, that can never finish on its own.
    ///
    /// A blocked or missing task strands everything downstream of it, however
    /// far downstream. Finding it is what makes the difference between "this
    /// task is waiting" and "this task will wait forever".
    ///
    /// Stops at a finished dependency: whatever *it* once waited on no longer
    /// matters.
    pub fn unreachable(&self, id: &str) -> Option<(&str, DepState)> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = self.deps(id).collect();

        while let Some(dep) = queue.pop_front() {
            if !seen.insert(dep) {
                continue;
            }
            match self.state(dep) {
                DepState::Dead => return Some((dep, DepState::Dead)),
                DepState::Unknown => return Some((dep, DepState::Unknown)),
                DepState::Done => {}
                DepState::Pending => queue.extend(self.deps(dep)),
            }
        }
        None
    }

    /// Whether `from` waits on `to`, directly or through other tasks.
    ///
    /// Two tasks in a dependency relation never run at the same time, however
    /// many hops apart they are — which is what makes an overlap in the files
    /// they touch safe rather than a collision waiting to happen.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = self.deps(from).collect();

        while let Some(next) = queue.pop_front() {
            if next == to {
                return true;
            }
            if !seen.insert(next) {
                continue;
            }
            queue.extend(self.deps(next));
        }
        false
    }

    /// How many tasks this one is holding up, transitively.
    ///
    /// The tie-break that decides which of two otherwise equal tasks to start:
    /// the one that releases more work.
    pub fn dependents(&self, id: &str) -> usize {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = self.dependents_of(id).collect();

        while let Some(next) = queue.pop_front() {
            if !seen.insert(next) {
                continue;
            }
            queue.extend(self.dependents_of(next));
        }
        seen.len()
    }

    fn dependents_of(&self, id: &str) -> impl Iterator<Item = &str> + use<'_> {
        self.reverse
            .get(id)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// Unfinished tasks in this task's group. A task with no group counts as one.
    pub fn group_open(&self, id: &str) -> usize {
        self.group_open.get(id).copied().unwrap_or(1)
    }

    /// How deep this task sits in the run: the longest chain of unfinished
    /// (not [`DepState::Done`]) dependencies standing above it, zero where it
    /// has none. What lets a group's block read top to bottom as run order —
    /// see [`crate::status::Row::key`] — since it is depth, not steps left on
    /// a task's own pipeline, that tells a dependency from its dependent.
    ///
    /// Zero for a task with no entry in `edges` at all — an id `depends_on`
    /// named that turned out archived or unknown, never a live task's own —
    /// and zero for a task inside a dependency cycle, which has no well-formed
    /// depth to report and would loop forever computing one.
    pub fn depth(&self, id: &str) -> usize {
        self.depth.get(id).copied().unwrap_or(0)
    }

    /// Whether a group has already produced work: any task of it has left
    /// `queued`, or any task of it names a finished `depends_on` that is not
    /// itself in the queue — the evidence, once every earlier task in the
    /// chain has been archived, that the group ran.
    ///
    /// Keyed by group name rather than task id — unlike [`Graph::group_open`]
    /// — because [`crate::dispatch`]'s gate asks this about a group a
    /// candidate does not belong to, not only about a task's own.
    pub fn group_is_open(&self, group: &str) -> bool {
        self.open_groups.contains(group)
    }

    /// The cycle this task takes part in, if any.
    pub fn cycle_with(&self, id: &str) -> Option<&[String]> {
        self.cycles
            .iter()
            .find(|cycle| cycle.iter().any(|member| member == id))
            .map(Vec::as_slice)
    }

    /// Whether the graph is one a task can actually get through: no cycles,
    /// and no dependency on a task that does not exist.
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = self.cycles.iter().map(|c| render_cycle(c)).collect();

        for (id, deps) in &self.edges {
            for dep in deps {
                if self.state(dep) == DepState::Unknown {
                    problems.push(format!("`{id}` depends on `{dep}`, which is not a task"));
                }
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            bail!("{}", problems.join("; "));
        }
    }

    /// One-line shape of the graph, for `spoolway doctor`.
    pub fn summary(&self) -> String {
        let edges: usize = self.edges.values().map(Vec::len).sum();
        format!("{} task(s), {edges} dependency edge(s)", self.edges.len())
    }
}

/// A cycle written the way it reads: `a → b → a`.
pub fn render_cycle(cycle: &[String]) -> String {
    let mut path: Vec<&str> = cycle.iter().map(String::as_str).collect();
    if let Some(first) = cycle.first() {
        path.push(first);
    }
    format!("dependency cycle {}", path.join(" → "))
}

/// Where a task stands, from the dispatcher's point of view.
///
/// One stage means finished, and it is a reserved one: `done`. Everything else
/// is either still going or stopped without finishing, and both hold dependents
/// where they are.
///
/// This used to read "any terminal that is not `blocked` is done", which was
/// the footgun that made terminals worth reserving: a third declared ending —
/// `superseded`, `rejected`, `abandoned` — would have released dependents on a
/// task that never finished, silently. Keyed off the one reserved name, a
/// project's own ending is `Dead` like any other stop, which is the safe answer
/// rather than the convenient one.
fn classify(task: &Task, pipelines: &Pipelines, unattended: bool) -> DepState {
    match task.stage() {
        crate::pipeline::DONE => return DepState::Done,
        crate::pipeline::BLOCKED => {
            // A staffed `blocked` step has a lane running on it, or about to —
            // still moving, not stranded. See [`DepState::Dead`].
            let staffed = pipelines
                .for_task(task)
                .is_ok_and(|p| p.blocked_is_staffed(unattended));
            return if staffed {
                DepState::Pending
            } else {
                DepState::Dead
            };
        }
        // Waiting on an approval, which is a wait and not a stop: the work at
        // the gated step succeeded and a person has only to let it past. Said
        // out loud rather than left to the fall-through below, because `Dead` is
        // the tempting reading and it is the wrong one twice over — it would
        // report every dependent as stranded behind a block, and it would tell
        // the person to run `spoolway resume` on a task that was never blocked.
        crate::pipeline::PAUSED => return DepState::Pending,
        _ => {}
    }
    let Ok(pipeline) = pipelines.for_task(task) else {
        return DepState::Pending;
    };
    match pipeline.step(task.stage()) {
        // A project's own declared ending. It stopped, and it is not `done`.
        Some(step) if step.kind() == StepKind::Terminal => DepState::Dead,
        _ => DepState::Pending,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Colour {
    White,
    Grey,
    Black,
}

/// Every cycle in the dependency edges, each reported once.
///
/// Plain depth-first search with the three-colour marking: reaching a node that
/// is still on the current path is a back edge, and the path from that node
/// onwards is the cycle. A task depending on itself is a cycle of one.
fn find_cycles(edges: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
    let mut colour: BTreeMap<&str, Colour> = edges
        .keys()
        .map(|id| (id.as_str(), Colour::White))
        .collect();
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut path: Vec<&str> = Vec::new();

    for root in edges.keys() {
        visit(root, edges, &mut colour, &mut path, &mut cycles);
    }

    // The same cycle is reachable from each of its members, so name each one by
    // its smallest id and keep one copy.
    for cycle in cycles.iter_mut() {
        if let Some(start) = cycle
            .iter()
            .enumerate()
            .min_by_key(|(_, id)| id.as_str())
            .map(|(index, _)| index)
        {
            cycle.rotate_left(start);
        }
    }
    cycles.sort();
    cycles.dedup();
    cycles
}

/// [`Graph::depth`] for every task `edges` knows about, computed once up
/// front the way `group_open` is.
///
/// Memoized depth-first search: a task's depth is one more than the deepest
/// of its own unfinished dependencies, or zero once none are left. `cycles`
/// is consulted first and its members short-circuited to zero before any
/// recursion — the only thing that makes the recursion provably finite,
/// since a task outside a cycle can still name one of its members as a
/// dependency.
fn compute_depths(
    edges: &BTreeMap<String, Vec<String>>,
    state: &BTreeMap<String, DepState>,
    cycles: &[Vec<String>],
) -> BTreeMap<String, usize> {
    let cyclic: BTreeSet<&str> = cycles
        .iter()
        .flat_map(|cycle| cycle.iter().map(String::as_str))
        .collect();

    let mut memo: BTreeMap<String, usize> = BTreeMap::new();
    for id in edges.keys() {
        depth_of(id, edges, state, &cyclic, &mut memo);
    }
    memo
}

fn depth_of(
    id: &str,
    edges: &BTreeMap<String, Vec<String>>,
    state: &BTreeMap<String, DepState>,
    cyclic: &BTreeSet<&str>,
    memo: &mut BTreeMap<String, usize>,
) -> usize {
    if let Some(depth) = memo.get(id) {
        return *depth;
    }
    // Cut the recursion here rather than let it find its own way back: a
    // cycle member can be reached from outside the cycle too, and only a
    // check made before recursing, not one made after, keeps every path in.
    if cyclic.contains(id) {
        memo.insert(id.to_string(), 0);
        return 0;
    }

    let mut deepest = 0;
    for dep in edges.get(id).into_iter().flatten() {
        if state.get(dep) == Some(&DepState::Done) {
            continue;
        }
        // A dependency with no edges of its own — archived or unknown, never
        // a live task — has nothing standing above it, so it contributes
        // depth zero rather than being skipped as unrecursable.
        let dep_depth = match edges.contains_key(dep) {
            true => depth_of(dep, edges, state, cyclic, memo),
            false => 0,
        };
        deepest = deepest.max(1 + dep_depth);
    }

    memo.insert(id.to_string(), deepest);
    deepest
}

fn visit<'a>(
    id: &'a str,
    edges: &'a BTreeMap<String, Vec<String>>,
    colour: &mut BTreeMap<&'a str, Colour>,
    path: &mut Vec<&'a str>,
    cycles: &mut Vec<Vec<String>>,
) {
    match colour.get(id) {
        Some(Colour::Black) => return,
        Some(Colour::Grey) => {
            if let Some(start) = path.iter().position(|step| *step == id) {
                cycles.push(path[start..].iter().map(|s| s.to_string()).collect());
            }
            return;
        }
        // An id with no task file of its own has no outgoing edges, so it can
        // close no cycle.
        None => return,
        Some(Colour::White) => {}
    }

    colour.insert(id, Colour::Grey);
    path.push(id);

    for dep in edges.get(id).into_iter().flatten() {
        // Borrow the key out of the map so the recursion keeps the map's
        // lifetime rather than the loop's.
        if let Some((key, _)) = edges.get_key_value(dep) {
            visit(key, edges, colour, path, cycles);
        }
    }

    path.pop();
    colour.insert(id, Colour::Black);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Frontmatter;
    use std::path::PathBuf;

    fn task(id: &str, stage: &str, depends_on: &[&str], group: Option<&str>) -> Task {
        Task {
            path: PathBuf::from(format!("{id}.md")),
            front: Frontmatter {
                id: id.into(),
                title: String::new(),
                stage: stage.into(),
                touches: vec![],
                depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
                parallel: false,
                borrowed: false,
                last_report: None,
                blocked_from: None,
                parked_from: None,
                resume: None,
                pipeline: None,
                group: group.map(str::to_string),
                source: None,
                plan: None,
                gate_at: None,
                branch: None,
                base: None,
                run: None,
                cut_from: None,
                base_commit: None,
                patch: None,
                skip: Vec::new(),
                trial: None,
                replay_of: None,
                worktree_path: None,
                workspace_id: None,
                pane_id: None,
                tab_id: None,
                attempts: 0,
                usage_limit_hold: false,
                parked_until: None,
                parked_window: String::new(),
                paused_at: None,
                launched_at: None,
                prompts: Default::default(),
                rounds: Default::default(),
                arrived_from: None,
                extra: Default::default(),
            },
            body: String::new(),
        }
    }

    fn graph(tasks: &[Task]) -> Graph {
        Graph::build(tasks, &Pipelines::builtin(), Path::new("/nonexistent"))
    }

    #[test]
    fn a_task_is_ready_only_once_every_dependency_is_terminal() {
        let tasks = [
            task("first", "implement", &[], None),
            task("second", "queued", &["first"], None),
        ];
        let graph = graph(&tasks);

        assert!(graph.ready("first"));
        assert!(!graph.ready("second"));
        assert_eq!(graph.waiting_on("second"), ["first"]);
        assert_eq!(graph.state("first"), DepState::Pending);
    }

    #[test]
    fn a_blocked_dependency_is_terminal_but_never_counts_as_done() {
        let tasks = [
            task("first", "blocked", &[], None),
            task("second", "queued", &["first"], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.state("first"), DepState::Dead);
        assert!(!graph.ready("second"));
        assert_eq!(graph.unreachable("second"), Some(("first", DepState::Dead)));
    }

    /// A staffed `blocked` step — materialised from `[unattended]`, and this
    /// run is unattended — has a lane running there or about to be, so it is
    /// still moving. Only `Graph::build_for_run` can tell: `Graph::build`
    /// always answers `Dead`, which is what the same task reads as under
    /// `Graph::build` with no run in particular.
    ///
    /// Nothing is added to the pipelines here: `Pipelines::builtin()` goes
    /// through `Pipelines::assemble`, which gives every pipeline a `blocked`
    /// step built from the config's `blocked_*` keys.
    #[test]
    fn a_staffed_blocked_dependency_is_pending_not_dead() {
        let pipelines = Pipelines::builtin();
        assert!(
            pipelines
                .pipelines
                .values()
                .all(|p| p.step(crate::pipeline::BLOCKED).is_some())
        );

        let tasks = [
            task("first", "blocked", &[], None),
            task("second", "queued", &["first"], None),
        ];

        let unattended = Graph::build_for_run(&tasks, &pipelines, Path::new("/nonexistent"), true);
        assert_eq!(unattended.state("first"), DepState::Pending);
        // Still not ready — `first` has not finished — but waiting quietly
        // rather than reported as stranded behind a block.
        assert!(!unattended.ready("second"));
        assert_eq!(unattended.unreachable("second"), None);

        let attended = Graph::build_for_run(&tasks, &pipelines, Path::new("/nonexistent"), false);
        assert_eq!(attended.state("first"), DepState::Dead);

        let no_run = graph(&tasks);
        assert_eq!(no_run.state("first"), DepState::Dead);
    }

    #[test]
    fn a_blocked_task_strands_everything_downstream_of_it() {
        let tasks = [
            task("a", "blocked", &[], None),
            task("b", "queued", &["a"], None),
            task("c", "queued", &["b"], None),
        ];
        let graph = graph(&tasks);

        // `c` names only `b`, which is merely waiting — the dead end is two
        // hops away and still has to be found.
        assert_eq!(graph.waiting_on("c"), ["b"]);
        assert_eq!(graph.unreachable("c"), Some(("a", DepState::Dead)));
    }

    #[test]
    fn a_dependency_on_a_task_that_does_not_exist_is_unreachable() {
        let tasks = [task("only", "queued", &["lgoin"], None)];
        let graph = graph(&tasks);

        assert_eq!(graph.state("lgoin"), DepState::Unknown);
        assert!(!graph.ready("only"));
        assert_eq!(
            graph.unreachable("only"),
            Some(("lgoin", DepState::Unknown))
        );
        assert!(
            graph
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not a task")
        );
    }

    #[test]
    fn a_finished_dependency_hides_whatever_it_once_waited_on() {
        let tasks = [
            task("a", "blocked", &[], None),
            task("b", "done", &["a"], None),
            task("c", "queued", &["b"], None),
        ];
        let graph = graph(&tasks);

        // `b` merged, so `a`'s state stopped mattering the moment it did.
        assert!(graph.ready("c"));
        assert_eq!(graph.unreachable("c"), None);
    }

    #[test]
    fn depth_counts_the_longest_run_of_unfinished_dependencies() {
        let tasks = [
            task("base", "done", &[], None),
            task("mid", "queued", &["base"], None),
            task("leaf", "queued", &["mid"], None),
        ];
        let graph = graph(&tasks);

        // `base` is done, so it stands above nothing that still counts.
        assert_eq!(graph.depth("mid"), 0);
        // `mid` is not done, so `leaf` sits one deeper than it.
        assert_eq!(graph.depth("leaf"), 1);
    }

    #[test]
    fn depth_is_zero_for_a_task_inside_a_dependency_cycle() {
        let tasks = [
            task("a", "queued", &["b"], None),
            task("b", "queued", &["a"], None),
            task("c", "queued", &["a"], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.depth("a"), 0);
        assert_eq!(graph.depth("b"), 0);
        // `c` depends on a cycle member, not on a peer inside it — its own
        // depth is still well-formed, one deeper than the zero the cycle read.
        assert_eq!(graph.depth("c"), 1);
    }

    #[test]
    fn a_cycle_is_found_and_named_once() {
        let tasks = [
            task("b", "queued", &["a"], None),
            task("a", "queued", &["b"], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.cycle_with("a").unwrap(), ["a", "b"]);
        assert_eq!(graph.cycle_with("b").unwrap(), ["a", "b"]);
        assert!(
            graph
                .validate()
                .unwrap_err()
                .to_string()
                .contains("a → b → a")
        );
    }

    #[test]
    fn a_task_depending_on_itself_is_a_cycle_of_one() {
        let tasks = [task("a", "queued", &["a"], None)];
        let graph = graph(&tasks);

        assert_eq!(graph.cycle_with("a").unwrap(), ["a"]);
        assert!(!graph.ready("a"));
    }

    #[test]
    fn a_long_chain_is_not_mistaken_for_a_cycle() {
        let tasks = [
            task("a", "queued", &[], None),
            task("b", "queued", &["a"], None),
            task("c", "queued", &["a", "b"], None),
        ];
        let graph = graph(&tasks);

        assert!(graph.cycle_with("c").is_none());
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn dependents_are_counted_through_the_whole_chain() {
        let tasks = [
            task("root", "queued", &[], None),
            task("mid", "queued", &["root"], None),
            task("leaf", "queued", &["mid"], None),
            task("other", "queued", &["root"], None),
            task("alone", "queued", &[], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.dependents("root"), 3, "mid, leaf and other");
        assert_eq!(graph.dependents("mid"), 1);
        assert_eq!(graph.dependents("alone"), 0);
    }

    #[test]
    fn counting_dependents_terminates_on_a_cycle() {
        let tasks = [
            task("a", "queued", &["b"], None),
            task("b", "queued", &["a"], None),
        ];
        assert_eq!(graph(&tasks).dependents("a"), 2, "a reaches b, and itself");
    }

    #[test]
    fn one_task_reaches_another_through_the_tasks_between_them() {
        let tasks = [
            task("schema", "queued", &[], None),
            task("api", "queued", &["schema"], None),
            task("ui", "queued", &["api"], None),
            task("unrelated", "queued", &[], None),
        ];
        let graph = graph(&tasks);

        assert!(
            graph.reaches("ui", "schema"),
            "two hops is still an ordering"
        );
        assert!(!graph.reaches("schema", "ui"), "the edge points one way");
        assert!(!graph.reaches("ui", "unrelated"));
        assert!(!graph.reaches("ui", "ui"));
    }

    #[test]
    fn reachability_terminates_on_a_cycle() {
        let tasks = [
            task("a", "queued", &["b"], None),
            task("b", "queued", &["a"], None),
        ];
        assert!(!graph(&tasks).reaches("a", "elsewhere"));
    }

    #[test]
    fn a_groups_open_count_ignores_what_has_already_merged() {
        let tasks = [
            task("big-1", "implement", &[], Some("big")),
            task("big-2", "queued", &[], Some("big")),
            task("big-3", "done", &[], Some("big")),
            task("small-1", "queued", &[], Some("small")),
            task("loner", "queued", &[], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.group_open("big-1"), 2);
        assert_eq!(graph.group_open("small-1"), 1);
        assert_eq!(graph.group_open("loner"), 1, "no group is a group of one");
    }

    /// A task's own count includes itself even once it has reached `done` —
    /// the state it is in for the one pass that fires its `done` hook, and
    /// the only pass where the number is read for
    /// `SPOOLWAY_GROUP_LAST`. Counting the group's *other* unfinished tasks
    /// instead would make every task the last one of its group.
    #[test]
    fn a_task_on_done_is_still_counted_in_its_own_group() {
        let tasks = [
            task("pair-a", "done", &[], Some("pair")),
            task("pair-b", "queued", &["pair-a"], Some("pair")),
            task("solo", "done", &[], Some("solo-group")),
            task("loner", "done", &[], None),
        ];
        let graph = graph(&tasks);

        assert_eq!(
            graph.group_open("pair-a"),
            2,
            "its sibling is still open, so it is not the group's last"
        );
        assert_eq!(
            graph.group_open("solo"),
            1,
            "nobody else in the group: this one really is the last"
        );
        assert_eq!(graph.group_open("loner"), 1, "no group is a group of one");
    }

    /// A chain's last remaining task has no live sibling to show its group
    /// ever ran — only its `depends_on` naming an archived task does. The
    /// archive check that tells `Done` from `Unknown` is what
    /// `group_is_open` leans on here, so this never touches the filesystem
    /// itself.
    #[test]
    fn a_queued_task_whose_dependency_is_archived_reads_as_an_open_group() {
        let tasks = [task("last", "queued", &["earlier"], Some("chain"))];
        let archive = crate::scratch::root("graph-open-group");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("earlier.md"), "").unwrap();

        let graph = Graph::build(&tasks, &Pipelines::builtin(), &archive);
        assert_eq!(graph.state("earlier"), DepState::Done);
        assert!(graph.group_is_open("chain"));
    }

    /// A dependency the archive has never heard of is `Unknown`, not `Done`
    /// — so it must never be read as evidence the group already ran.
    #[test]
    fn a_missing_dependency_does_not_open_the_group() {
        let tasks = [task("only", "queued", &["nowhere"], Some("chain"))];
        let graph = graph(&tasks);

        assert_eq!(graph.state("nowhere"), DepState::Unknown);
        assert!(!graph.group_is_open("chain"));
    }

    /// A task that has left `queued` opens its group even with no finished
    /// dependency to point at — the ordinary case, not the archived-chain one.
    #[test]
    fn a_task_past_queued_opens_its_group() {
        let tasks = [
            task("running", "implement", &[], Some("chain")),
            task("waiting", "queued", &[], Some("chain")),
        ];
        let graph = graph(&tasks);

        assert!(graph.group_is_open("chain"));
    }

    /// `group:` is read verbatim, never path-parsed — the whole reason it
    /// replaced `plan:`. Two tasks naming the same GitHub issue URL group
    /// together; a path that merely shares a file stem with a bare word does
    /// not, which is the collision `plan_slug`'s `file_stem` call used to
    /// cause.
    #[test]
    fn group_is_read_verbatim_and_never_path_parsed() {
        let tasks = [
            task(
                "one",
                "queued",
                &[],
                Some("https://github.com/x/y/issues/42"),
            ),
            task(
                "two",
                "queued",
                &[],
                Some("https://github.com/x/y/issues/42"),
            ),
            task("three", "queued", &[], Some("plans/big.html")),
            task("four", "queued", &[], Some("big")),
        ];
        let graph = graph(&tasks);

        assert_eq!(graph.group_open("one"), 2, "same URL, same group");
        assert_eq!(
            graph.group_open("three"),
            1,
            "a path and a bare word sharing a file stem are different groups now"
        );
        assert_eq!(graph.group_open("four"), 1);
    }
}
