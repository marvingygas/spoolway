//! Giving a task's checkout back: panes and workspaces closed, worktrees
//! removed, branches deleted or kept.
//!
//! Two moments call this, and they disagree on one thing: a task that
//! reached `done` has finished, so its branch is spent litter the moment
//! nothing else still needs it ([`Branch::Delete`], from
//! [`Dispatcher::clean_up`]); a task swept because the run itself is
//! stopping has not finished, so its file stays queued and the branch that
//! holds its commits must stay too ([`Branch::Keep`], from
//! [`Dispatcher::sweep_on_stop`]). Everything below either function calls
//! is shared between the two: which parts of a checkout exist to give back,
//! and in what order.
//!
//! Kept apart from the rest of [`crate::dispatch`] because this is the one
//! cluster of it that ends a task's residence in the queue (or the run's
//! hold on it) rather than moving it along a pipeline — reconciling stage
//! against pipeline is `dispatch`'s job, and undoing what a checkout holds
//! is this module's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::dispatch::{Dispatcher, LaneRecord, Report, measure_patch, now_secs, save_lane_records};
use crate::mux::{Lane, lane_name, parse_lane_name};
use crate::task::Task;

/// What becomes of a task's branch when its checkout is given back.
///
/// The distinction is whether the task *finished*. Once its work is merged or
/// published the branch is a spent local ref, and leaving it behind is litter.
/// Interrupt the same task mid-step and that branch is the only place its
/// commits exist — deleting it there throws the work away silently, since the
/// task file stays in the queue and says nothing about having been reset.
#[derive(PartialEq, Eq)]
pub(crate) enum Branch {
    /// The task is done and is being archived.
    Delete,
    /// The task was interrupted and stays queued. The next run finds the branch
    /// still there and cuts a fresh worktree on it rather than from base, so the
    /// step resumes on top of what it had rather than starting over.
    Keep,
}

impl<'a> Dispatcher<'a> {
    /// A task reached a terminal step that cleans up: tear down its worktree
    /// and branch, and move its file out of the active queue.
    pub(crate) fn clean_up(
        &mut self,
        task: &mut Task,
        owned: &[(String, String, &Lane)],
        report: &mut Report,
    ) -> Result<bool> {
        if self.dry_run {
            report.actions.push(format!("would clean up {}", task.id()));
            return Ok(false);
        }

        for (_, task_id, lane) in owned {
            if task_id == task.id() {
                let _ = self.mux.stop_lane(&lane.name, &lane.pane_id);
                self.lanes.remove(&lane.name);
            }
        }

        // A background command was left running on purpose, and the worktree
        // about to be removed is where it is running. Stopped before that
        // happens, or it spends the rest of its life writing into a directory
        // that no longer exists. A pane a failing run left standing goes with
        // it too — the task is leaving for good, so there is no later arrival
        // left to replace it.
        let runs = crate::command_step::Runs::new(&self.repo.commands_dir());
        for key in runs.keys_for_task(task.id()) {
            if let Some(pane) = runs.pane(&key) {
                let _ = self.mux.close_pane(&pane);
                runs.forget_pane(&key);
            }
            runs.stop(&key);
        }

        // The last instant the branch still exists to diff against — a task
        // with no `base_commit` (a borrowed checkout, or one archived before
        // this was recorded) has nothing to measure against and is left
        // without a `patch` rather than guessed at.
        if let (Some(base_commit), Some(worktree)) =
            (&task.front.base_commit, &task.front.worktree_path)
        {
            task.front.patch = measure_patch(worktree, base_commit);
            // The file on disk is what gets renamed into the archive below —
            // written here, or the patch just measured never leaves memory.
            task.save()?;
        }

        self.tear_down_checkout(task, Branch::Delete);

        let destination = self.repo.archive_dir().join(format!("{}.md", task.id()));
        std::fs::create_dir_all(self.repo.archive_dir())?;
        std::fs::rename(&task.path, &destination).with_context(|| {
            format!(
                "archiving {} to {}",
                task.path.display(),
                destination.display()
            )
        })?;

        self.close_project_tab_if_empty(task);

        // `task` itself just moved to the archive, so this reread is what
        // lets a branch retained for *its* sake, earlier in the chain, be
        // freed the moment it turns out nothing needs it any more.
        let remaining = self.repo.tasks().unwrap_or_default();
        self.sweep_orphaned_branches(&remaining);

        report
            .actions
            .push(format!("{}: cleaned up and archived", task.id()));
        Ok(true)
    }

    /// Close this project's shared tab, once the project has nothing left in
    /// the queue.
    ///
    /// Called with the task already archived, so what is left to read is
    /// exactly what the project still has to do: a sibling still queued,
    /// still running or still blocked is a pane this tab is about to hold
    /// again, and closing it under them would take a live lane with it.
    ///
    /// Under `split` there is nothing here to do — the task's own workspace
    /// is already gone with [`Dispatcher::tear_down_checkout`], and the tab
    /// recorded on it went with it.
    fn close_project_tab_if_empty(&mut self, task: &Task) {
        if self.mux.task_owns_workspace() {
            return;
        }
        let Some(tab_id) = task.front.tab_id.clone() else {
            return;
        };
        let left = self.repo.tasks().unwrap_or_default();
        if left.iter().any(|other| other.id() != task.id()) {
            return;
        }
        let _ = self.mux.close_tab(&tab_id);
    }

    /// Give back everything a task's checkout is holding: its workspace, the
    /// worktree under it, and — if [`Branch::Delete`] says so — the local
    /// branch it was cut on.
    ///
    /// Split out of [`Dispatcher::clean_up`] because the stop sweep needs
    /// exactly this and none of the rest of it: a task swept because the run
    /// was interrupted has not *finished*, so its file stays in the queue to be
    /// picked up again rather than being archived as though it had.
    pub(crate) fn tear_down_checkout(&mut self, task: &mut Task, branch: Branch) {
        // Whether the workspace recorded on this task is the task's own or the
        // one the whole run shares — see [`Mux::task_owns_workspace`]. Under
        // `grouped` every task is a pane in the tab its project shares, so
        // there is nothing of the task's own to close here at all — its lane
        // pane is already stopped by the time this runs, and the shared tab
        // is not this task's to close.
        let owns_workspace = self.mux.task_owns_workspace();

        // The checkout this task cut for itself, if it cut one. A borrowed
        // checkout is somebody else's and is never removed here, whatever else
        // happens below. Read from what was recorded when the lane was set up,
        // never probed now — a worktree we cut and one we borrowed look
        // identical from here.
        let own_checkout = match task.front.borrowed {
            true => None,
            false => task.front.worktree_path.clone(),
        };

        if owns_workspace && let Some(workspace) = task.front.workspace_id.clone() {
            // Which call this is matters more than it looks: one removes the
            // worktree under the workspace, and a borrowed workspace is pointed
            // at somebody's own checkout. Already gone is a fine outcome, not
            // an error.
            match task.front.borrowed {
                false => {
                    // `remove_workspace` is meant to take the worktree and the
                    // row above it in one call, and does whenever the
                    // multiplexer still holds the two together. It does not
                    // always: herdr binds a worktree to a workspace only for a
                    // workspace opened *onto* the checkout, so a row that
                    // reached the checkout any other way — an older build's
                    // resume, a row a person reopened by hand — comes back
                    // unbound and the call answers `not_linked_worktree`,
                    // removing nothing.
                    //
                    // So the answer is read rather than dropped. Dropping it is
                    // how a finished task used to leave both its worktree and a
                    // stray row standing, and both then outlived the whole run.
                    // What is left when it says no is taken apart by hand: the
                    // worktree with git, which needs no binding to remove, and
                    // then the row on its own.
                    if self.mux.remove_workspace(&workspace).is_err() {
                        if let Some(checkout) = &own_checkout {
                            let _ = self.mux.remove_checkout(checkout);
                        }
                        let _ = self.mux.close_workspace(&workspace);
                    }
                }
                true => {
                    // The session may be shared with another task pointed at
                    // the very same borrowed checkout — see
                    // [`crate::mux::Mux::create_pane`] — so only this task's
                    // own tab is ours to close. Failing to close it means it
                    // was the workspace's only tab, which the multiplexer
                    // refuses to leave empty, and which means nobody else was
                    // sharing it — so the whole thing goes instead.
                    let tab_closed = task
                        .front
                        .tab_id
                        .as_deref()
                        .is_some_and(|tab| self.mux.close_tab(tab).is_ok());
                    if !tab_closed {
                        let _ = self.mux.close_workspace(&workspace);
                    }
                }
            }
        } else if let Some(checkout) = &own_checkout {
            // Nothing above took the checkout with it, and it is still
            // spoolway's own cut. Either this run groups its tasks into one
            // shared tab, so the worktree was made with git directly and has no
            // row of its own to go with — see
            // [`crate::mux::Mux::create_workspace`] — or the task holds a
            // worktree with no workspace recorded against it at all, which used
            // to leak for want of anywhere to hang the removal.
            let _ = self.mux.remove_checkout(checkout);
        }

        self.reclaim_scratch(task.id());

        // Only a branch spoolway made is spoolway's to delete, and only here,
        // where it was cut. A borrowed checkout's branch was cut by a person
        // and is theirs to remove — a closeout's `branch:` is the plan branch
        // it merged, and deleting that out from under whoever has it checked
        // out would be its own bug.
        //
        // Local only. The published branch belongs to the forge, which deletes
        // a merged pull request's head branch itself if the project asked it
        // to — a repository setting, and not something to reimplement one
        // `git push --delete` at a time. Under `person` the pull request may
        // still be open here, and deleting its branch would close it.
        // Something queued still has to be cut from this branch — see
        // `Dispatcher::branch_still_needed`. Deleting it now would leave that
        // dependent with nothing to start from; `Dispatcher::sweep_orphaned_branches`
        // is what frees it later, once the last such task has been cut.
        let depended_on = branch == Branch::Delete && self.branch_still_needed(task.id());
        let made_its_branch = !task.front.borrowed && branch == Branch::Delete && !depended_on;
        if let Some(branch) = task.front.branch.clone().filter(|_| made_its_branch) {
            // `-D` rather than `-d`: the branch may have been squash-merged, so
            // git considers it "not fully merged" even though its content is in
            // — and under `person` it may not be merged at all yet, which is
            // the deal that mode signs.
            let _ = self.repo.git(&["branch", "-D", &branch]);
        }
    }

    /// Whether some task still in the queue names `id` in its own
    /// `depends_on` — the branch `id` was cut on is what that task's own
    /// worktree still has to be cut from, whatever stage `id` itself is on.
    fn branch_still_needed(&self, id: &str) -> bool {
        self.repo
            .tasks()
            .unwrap_or_default()
            .iter()
            .any(|t| t.front.depends_on.iter().any(|dep| dep == id))
    }

    /// Delete every local `task/<id>` branch whose task has already finished
    /// — there is no `<id>.md` left in the queue — and that nothing still
    /// queued names in `depends_on` any more.
    ///
    /// Reaching `done` retains a depended-on branch rather than deleting it,
    /// and nothing revisits that task once its file has moved to the
    /// archive — so the branch is only ever freed by *another* task's own
    /// cleanup finding it now unneeded. Cheap enough to run on every cleanup
    /// rather than tracked separately: one `for-each-ref` and a scan of the
    /// tasks already in hand.
    fn sweep_orphaned_branches(&mut self, tasks: &[Task]) {
        if self.dry_run {
            return;
        }
        let Ok(listing) = self.repo.git(&[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/task",
        ]) else {
            return;
        };
        for branch in listing.lines() {
            let Some(id) = branch.strip_prefix("task/") else {
                continue;
            };
            // Still in the queue: this task owns its branch until it reaches
            // `done` itself, whatever else is going on around it.
            if self.repo.queue_dir().join(format!("{id}.md")).exists() {
                continue;
            }
            // Something still queued has not been cut from it yet.
            if tasks
                .iter()
                .any(|t| t.front.depends_on.iter().any(|dep| dep == id))
            {
                continue;
            }
            // Only a branch spoolway made, for a task that actually
            // finished, is spoolway's to delete — read back from the
            // archive, since a borrowed checkout's own frontmatter is the
            // only record of that once the task's file has moved.
            let archived = self.repo.archive_dir().join(format!("{id}.md"));
            let Ok(finished) = Task::load(&archived) else {
                continue;
            };
            if finished.front.borrowed {
                continue;
            }
            let _ = self.repo.git(&["branch", "-D", branch]);
        }
    }

    /// Take back the scratch directory a step allowed to `rebase` was given.
    ///
    /// A rebase there is done in a worktree of its own, on a branch of its own,
    /// and both are registered in the repo the lane borrowed them from — so
    /// leaving them behind litters someone's branch list with one `-rebase`
    /// entry per task, forever. Only worktrees inside *this task's* scratch
    /// directory are removed, and only the branches those worktrees had checked
    /// out, so nothing a person made is ever a candidate.
    pub(crate) fn reclaim_scratch(&self, task_id: &str) {
        let scratch = self.repo.scratch_dir().join(task_id);
        if !scratch.exists() {
            return;
        }

        // Canonicalised once, ahead of the loop: `path` below is whatever git
        // registered for the worktree, which need not be spelled the same
        // way `scratch` is here — a symlink in the temp root a scratch
        // directory sits under is enough to make `starts_with` miss a real
        // match. A miss used to fall through silently to `remove_dir_all`
        // below, which deletes the worktree's checkout without ever telling
        // git to let go of it first — no `worktree remove`, so no `branch
        // -D` either, and the branch this function exists to clean up is
        // left behind, exactly the litter its own doc comment describes.
        // One spelling for both sides of the `starts_with` below. On Windows
        // `canonicalize` answers in verbatim form — `\\?\C:\…` — while the
        // fallback for a path that no longer exists keeps its raw spelling,
        // so a gone worktree under a live scratch directory compares a bare
        // drive path against a prefixed one and never matches. Stripping the
        // prefix from whichever side grew it puts both in the raw form.
        fn comparable(path: &Path) -> PathBuf {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            match canonical.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
                Some(raw) => PathBuf::from(raw),
                None => canonical,
            }
        }
        let canonical_scratch = comparable(&scratch);

        let listing = self
            .repo
            .git(&["worktree", "list", "--porcelain"])
            .unwrap_or_default();

        let mut path: Option<&str> = None;
        for line in listing.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(rest);
            } else if let Some(reference) = line.strip_prefix("branch ") {
                // `canonicalize` fails on a worktree whose checkout git still
                // lists but whose directory is already gone — the exact
                // "prunable" case this function exists to clean up after.
                // Falling back to `false` there, the way a first pass at this
                // fix did, refuses the match and leaves the branch behind
                // just as the uncanonicalised comparison this replaced did
                // for a symlinked path; falling back to the raw path instead
                // keeps a removed checkout matching, the same as
                // `canonical_scratch` above already falls back for `scratch`
                // itself.
                let inside = path
                    .map(|p| comparable(Path::new(p)).starts_with(&canonical_scratch))
                    .unwrap_or(false);
                if !inside {
                    continue;
                }
                let branch = reference.trim_start_matches("refs/heads/");
                if let Some(p) = path {
                    let _ = self.repo.git(&["worktree", "remove", "--force", p]);
                }
                let _ = self.repo.git(&["branch", "-D", branch]);
            }
        }

        let _ = std::fs::remove_dir_all(&scratch);
        let _ = self.repo.git(&["worktree", "prune"]);
    }

    /// Give back what the run is still holding, because it is stopping.
    ///
    /// A task that reached `done` tore its own checkout down on the way, so
    /// what is left here is whatever was still in flight when the person
    /// stopped — and one kind of leftover is deliberate. **A task parked in
    /// front of a person — `paused`, or `blocked` with nobody staffed to
    /// answer it — is kept whole.** Its pane is being held open for someone
    /// to read, and `spoolway resume` resumes it against the checkout
    /// underneath: sweeping that would answer a question by deleting it.
    /// See [`Dispatcher::parked_for_a_person`].
    ///
    /// Nothing swept here is archived, and nothing swept here loses its work. A
    /// task that was interrupted has not finished, so its file stays in the
    /// queue and its branch stays in the repository: the worktree is what the
    /// run was holding, and the commits on that branch are what the agent got
    /// done before you stopped it. The next run cuts a fresh worktree on the
    /// branch it finds — see [`Branch::Keep`].
    ///
    /// The dispatch workspace goes last, and only if nothing was spared: a
    /// blocked lane still in it is the whole reason the workspace is worth
    /// keeping open.
    ///
    /// **The lane itself is ended before its checkout goes**, the same order
    /// [`Dispatcher::clean_up`] uses for a task that reaches `done`. Under
    /// `grouped` tearing the checkout down is nothing but `git worktree
    /// remove --force` — it never touches the pane — so without this an
    /// agent still running there would find its directory gone out from
    /// under it, and any `background: true` command it started would keep
    /// running there too. Only `split`, where taking the workspace apart
    /// happens to take the pane with it, ever got away without this.
    ///
    /// Two things travel with the worktree, and both used to be dropped here.
    /// The lane's spend is banked before its record goes, because an
    /// interrupted lane spent exactly as many tokens as one that finished and
    /// they were otherwise lost from the accounting entirely. And the launch
    /// counter is forgiven, because the lane it counted is one this sweep is
    /// taking apart — see [`Task::launch_landed`]. Without that, every task in
    /// flight when a person stops a dispatcher is `blocked` when they start the
    /// next one, which is the opposite of what `dispatch.tear_lanes_on_stop`
    /// promises.
    pub fn sweep_on_stop(&mut self, report: &mut Report) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let mut tasks = self.repo.tasks()?;

        // The live lanes this run owns, read fresh from the multiplexer the
        // same way `pass` does — `self.lanes` is only the usage bookkeeping,
        // and has no pane id to stop a lane with.
        let step_ids = self.pipelines.all_step_ids();
        let all_lanes = self.mux.list_lanes()?;
        let mine = crate::dispatch::our_checkouts(self.repo, &tasks);
        let owned: Vec<(String, &Lane)> = all_lanes
            .iter()
            .filter(|lane| mine.contains(&lane.cwd))
            .filter_map(|lane| {
                parse_lane_name(&lane.name, &step_ids).map(|(_, task)| (task.to_string(), lane))
            })
            .collect();

        let mut spared = 0usize;
        let mut swept = 0usize;
        let mut ended = 0usize;
        // The project's shared tab is closed once the sweep has emptied it,
        // and kept the moment any task is spared: the tab is where that
        // parked lane — paused, or blocked with nobody staffed to answer it —
        // is being held open for a person to read. Every task left in this
        // repo's queue is this one project's, so there is only ever the one
        // tab to track.
        let mut project_tab_id: Option<String> = None;
        let mut kept_any = false;

        for task in &mut tasks {
            if self.parked_for_a_person(task) {
                // Only counts as something to keep the workspace open for if it
                // is actually in there — a parked task that never got as far
                // as a checkout is holding nothing.
                spared += usize::from(task.front.workspace_id.is_some());
                kept_any = true;
                continue;
            }
            if task.front.workspace_id.is_none() && task.front.worktree_path.is_none() {
                continue;
            }

            // Before the teardown: `record_usage` reads the task for the plan
            // and the step's own verdict, and the step is the stage this task
            // is about to be left sitting on.
            let step_id = task.stage().to_string();
            let name = lane_name(&step_id, task.id());
            // `readopted` rather than a plain fallback to a blank record: this
            // stop's own dispatcher may itself be a restart of one that died
            // mid-pass, in which case `self.lanes` never had this lane's
            // record to begin with — see `LaneRecord::readopted`.
            // `record_usage` is a safe no-op either way, on the blank session
            // a lane the ledger has genuinely never heard from still gets.
            let record = self
                .lanes
                .remove(&name)
                .unwrap_or_else(|| LaneRecord::readopted(self.repo, &name, now_secs()));
            let pipeline = self
                .pipelines
                .for_task(task)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            self.record_usage(&record, task.id(), &step_id, Some(task), &pipeline);

            // The lane and any background command it left running, stopped
            // before the checkout under them goes — see [`Dispatcher::clean_up`],
            // which this copies the order of.
            for (_, lane) in owned.iter().filter(|(task_id, _)| task_id == task.id()) {
                let _ = self.mux.stop_lane(&lane.name, &lane.pane_id);
                ended += 1;
            }
            let runs = crate::command_step::Runs::new(&self.repo.commands_dir());
            for key in runs.keys_for_task(task.id()) {
                if let Some(pane) = runs.pane(&key) {
                    let _ = self.mux.close_pane(&pane);
                    runs.forget_pane(&key);
                }
                runs.stop(&key);
            }

            self.tear_down_checkout(task, Branch::Keep);
            if let Some(tab_id) = task.front.tab_id.clone() {
                project_tab_id = Some(tab_id);
            }
            task.front.workspace_id = None;
            task.front.pane_id = None;
            task.front.tab_id = None;
            task.front.worktree_path = None;
            task.launch_landed();
            task.save()?;
            swept += 1;
        }

        // The records the sweep consumed are gone from memory; the file they
        // came from is what the next dispatcher reads. Nothing else writes it
        // on this path — a stop happens between passes, and `pass` is what
        // ordinarily saves them.
        save_lane_records(self.repo, &self.lanes)?;

        if swept > 0 {
            report.actions.push(format!(
                "stopping: ended {ended} lane(s) and gave back {swept} worktree(s)"
            ));
        }

        // The project's tab closes only once the sweep has emptied it and
        // nothing of it was spared — never the shared workspace itself,
        // which may hold another project's live lanes, and only a person
        // closes that row.
        //
        // Under `split` there is no project tab to close: a task's own
        // workspace went with its teardown, and `tab_id` there names a tab
        // of its own that went with it.
        if !self.mux.task_owns_workspace()
            && !kept_any
            && let Some(tab_id) = project_tab_id
        {
            let _ = self.mux.close_tab(&tab_id);
        }

        if spared > 0 {
            report.actions.push(format!(
                "stopping: left {spared} blocked task(s) and the project's tab holding them"
            ));
        }

        Ok(())
    }
}
