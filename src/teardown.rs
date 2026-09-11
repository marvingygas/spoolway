//! Giving a task's checkout back: panes and workspaces closed, worktrees
//! removed, its branch deleted.
//!
//! One moment calls this: a task that reached `done` has finished, so its
//! branch is spent litter the moment nothing else still needs it — see
//! [`Dispatcher::clean_up`]. Stopping the dispatcher itself never reaches
//! here any more; [`Dispatcher::sweep_on_stop`] banks an interrupted lane's
//! spend and forgives its launch counter without touching its checkout,
//! since the task has not finished and the next run resumes it exactly where
//! it stood. Even a finished task's branch is spared while some remote still
//! lacks a commit of it — the keep is named on the run's problem list — see
//! the push check in [`Dispatcher::tear_down_checkout`] below. Everything
//! below is which parts of a checkout exist to give back, and in what order.
//!
//! Kept apart from the rest of [`crate::dispatch`] because this is the one
//! cluster of it that ends a task's residence in the queue rather than
//! moving it along a pipeline — reconciling stage against pipeline is
//! `dispatch`'s job, and undoing what a checkout holds is this module's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::dispatch::{Dispatcher, LaneRecord, Report, measure_patch, now_secs, save_lane_records};
use crate::mux::{Lane, lane_name};
use crate::task::Task;

impl<'a> Dispatcher<'a> {
    /// A task reached a terminal step that cleans up: bank whatever its lanes
    /// spent, tear down its worktree and branch, and move its file out of the
    /// active queue.
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

        // Bank each owned lane's spend before its record goes, the way
        // `sweep_on_stop` does. A pipeline whose last agent step routes
        // `on_pass: done` has its lane still writing when the task reaches
        // here; `free_finished_lanes` skipped it as busy, and without this
        // its tokens were killed unbanked (review finding 14).
        //
        // The step banked is the lane's own — `owned`'s first tuple element —
        // not `task.stage()`. By the time `clean_up` runs the task has
        // already been moved onto its terminal step, so `task.stage()` would
        // stamp the line `done` for tokens the previous agent step spent, and
        // `lane_name(&entry.step, &entry.task)` — which `LaneRecord::readopted`
        // and `dispatch::lane_session` both reconstruct — would name a lane
        // that never existed. `sweep_on_stop` derives its lane name from the
        // stage too, so stage and lane always agree there; here they do not.
        let pipeline = self
            .pipelines
            .for_task(task)
            .map(|p| p.name.clone())
            .unwrap_or_default();

        // The banked records are kept in hand rather than dropped: their
        // per-session agent homes are reclaimed further down, but only once
        // the task is actually archived. `clean_up` can still turn back below
        // and hold the task at `blocked` (uncommitted work), and a `session:`
        // step resuming it then would want that transcript.
        let mut banked: Vec<LaneRecord> = Vec::new();
        for (step_id, task_id, lane) in owned {
            if task_id == task.id() {
                let _ = self.mux.stop_lane(&lane.name, &lane.pane_id);
                let ledger = self.ledger();
                let record = self
                    .lanes
                    .remove(&lane.name)
                    .unwrap_or_else(|| LaneRecord::readopted(&lane.name, now_secs(), &ledger));
                self.record_usage(&record, task.id(), step_id, Some(task), &pipeline);
                banked.push(record);
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

        // The worktree is about to be removed. If it still holds work
        // `auto_commit` cannot record — a git command failing, or
        // `dispatch.auto_commit` off — tearing it down destroys that work,
        // against this function's own contract that a cleanup terminal
        // "cannot delete work that was never recorded" (review finding 4).
        // Held at `blocked` instead. Residue a lane deliberately left is not
        // this: it is named in the status log and backed up nowhere, and
        // cleanup knowingly discards it rather than turning it into task work.
        //
        // `""` for `started_at`: there is no lane record to read a launch HEAD
        // from by the time cleanup runs, and an unknown one makes `auto_commit`
        // sweep rather than treat the leftovers as residue — the same call
        // `spoolway stack` already makes at `handover`.
        if let Some(worktree) = task.front.worktree_path.clone() {
            let step = task
                .front
                .last_report
                .as_ref()
                .map(|r| r.step.clone())
                .unwrap_or_else(|| task.stage().to_string());
            let outcome =
                crate::commands::auto_commit(self.repo, Path::new(&worktree), "", task.id(), &step);
            if let Some(note) = outcome.note() {
                task.append_to_section("## Status Log", &format!("- {note}\n"));
            }
            if outcome.is_unrecorded() {
                let back = self
                    .pipelines
                    .for_task(task)
                    .ok()
                    .map(|pipeline| crate::commands::resume_target(task, pipeline))
                    .or_else(|| task.front.last_report.as_ref().map(|r| r.step.clone()))
                    .unwrap_or_else(|| task.stage().to_string());
                crate::commands::set_blocked_from(task, &back);
                task.append_to_section(
                    "## Status Log",
                    &format!(
                        "- reached `{}` with work that could not be committed — held at `{}` \
                         rather than tearing the worktree down\n",
                        task.stage(),
                        crate::pipeline::BLOCKED,
                    ),
                );
                task.set_stage(
                    crate::pipeline::BLOCKED,
                    Some("uncommitted work could not be recorded before cleanup"),
                );
                task.save()?;
                report.problems.push(format!(
                    "{}: held at `blocked` — uncommitted work could not be recorded before cleanup",
                    task.id()
                ));
                return Ok(false);
            }
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

        self.tear_down_checkout(task, report);

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

        // The task has left the queue for good. Its hook and command run
        // files under `tracking/` and `commands/` are litter now, and left
        // in place `tracking::failure_count` would go on counting a failed
        // hook of a task nobody can reach any more (review finding 64).
        runs.reclaim_task(task.id());
        crate::tracking::reclaim(self.repo, task.id());

        // And now — past every early return — the per-session agent homes the
        // task's lanes were given: the ones just banked above, plus any older
        // lane record still held for it. A copied `auth.json` under one would
        // otherwise outlive every credential rotation (review finding 63).
        for record in &banked {
            record.reclaim_session_home();
        }
        let stale: Vec<String> = self
            .lanes
            .keys()
            .filter(|name| crate::mux::lane_task(name) == task.id())
            .cloned()
            .collect();
        for name in stale {
            if let Some(record) = self.lanes.remove(&name) {
                record.reclaim_session_home();
            }
        }

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
    /// worktree under it, and — if every commit on it has reached a remote —
    /// the local branch it was cut on.
    ///
    /// Called only from [`Dispatcher::clean_up`], once a task has actually
    /// finished — stopping the dispatcher never reaches here any more, since
    /// [`Dispatcher::sweep_on_stop`] leaves an interrupted task's checkout
    /// exactly where it stood.
    ///
    /// `report` is where a branch kept because it is not fully pushed gets
    /// named — see the comment on the delete itself, below.
    pub(crate) fn tear_down_checkout(&mut self, task: &mut Task, report: &mut Report) {
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
        let depended_on = self.branch_still_needed(task.id());
        let made_its_branch = !task.front.borrowed && !depended_on;
        if let Some(branch) = task.front.branch.clone().filter(|_| made_its_branch) {
            // `-D` rather than `-d`: the branch may have been squash-merged, so
            // git considers it "not fully merged" even though its content is in
            // — and under `person` it may not be merged at all yet, which is
            // the deal that mode signs. That same squash-merge is why git's own
            // "is this merged" check cannot stand in for the question asked
            // here either: the branch is asked directly whether every commit
            // on it is reachable from some remote-tracking ref. A `handover`
            // that never ran, or a push that failed silently, is otherwise the
            // last thing that happens to a finished task being the deletion of
            // the only copy of its work — which is exactly how one task lost
            // twelve commits to this path.
            if self.branch_fully_pushed(&branch) {
                let _ = self.repo.git(&["branch", "-D", &branch]);
            } else {
                report.problems.push(format!(
                    "{}: kept branch `{branch}` — it has commits no remote has",
                    task.id()
                ));
            }
        }
    }

    /// Whether every commit reachable from `branch` is also reachable from
    /// some remote-tracking ref — not necessarily the same-named upstream,
    /// just some `refs/remotes/*` ref, the way `sweep_orphaned_branches`
    /// below reads the same question for a branch this check already kept.
    ///
    /// A git failure here — no such branch, a corrupt ref, a repository with
    /// no remote at all — reads as "not pushed" rather than propagating: this
    /// gates whether the only copy of a task's work gets deleted, so the safe
    /// side of any doubt is to keep it.
    fn branch_fully_pushed(&self, branch: &str) -> bool {
        match self.repo.git(&["rev-list", branch, "--not", "--remotes"]) {
            Ok(unpushed) => unpushed.trim().is_empty(),
            Err(_) => false,
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

    /// Delete every local `task/…` branch whose task has already finished —
    /// there is no `<id>.md` left in the queue — and that nothing still
    /// queued names in `depends_on` any more. The branch is `task/<id>`, or
    /// `task/<slug>-<id>` when `issue_tracking.key_in_names` prefixed it;
    /// either way the owning task is found by an exact match against its
    /// recorded `branch:` field — see [`task_for_branch`] — not by taking the
    /// branch name apart.
    ///
    /// Reaching `done` retains a depended-on branch rather than deleting it,
    /// and nothing revisits that task once its file has moved to the
    /// archive — so the branch is only ever freed by *another* task's own
    /// cleanup finding it now unneeded. Cheap enough to run on every cleanup
    /// rather than tracked separately: one `for-each-ref` and a scan of the
    /// tasks already in hand.
    ///
    /// The same push check `tear_down_checkout` used to spare this branch in
    /// the first place is asked again here, for the same reason: nothing
    /// about becoming orphaned makes an unpushed commit any less the only
    /// copy of the work it holds. Once it is pushed, this is what frees it —
    /// nothing re-visits an archived task's own cleanup to do it there.
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
            // The task that recorded this exact branch, if any is still on
            // disk. A branch no queue or archive task claims — its task swept
            // from the archive by `retention.days`, say — is left alone.
            let Some(owner) = task_for_branch(self.repo, branch) else {
                continue;
            };
            let id = owner.id();
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
            // finished, is spoolway's to delete — the archived record is the
            // only place `borrowed` is still readable once the task's file
            // has moved, and `task_for_branch` found `owner` there.
            if owner.front.borrowed {
                continue;
            }
            if !self.branch_fully_pushed(branch) {
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

    /// Settle the books on what the run was still holding, because it is
    /// stopping — without touching any of it.
    ///
    /// A task that reached `done` tore its own checkout down already, so what
    /// is left here is whatever was still in flight when the run stopped. None
    /// of it is removed: no worktree, no workspace, no pane, no tab. The task
    /// stays exactly on the stage it was mid-step on, its checkout stays where
    /// it was cut, and its lane — agent and any `background: true` command
    /// alike — is left running, whatever backend it runs under. There is
    /// nothing here for the next run to resume *into*; it is already sitting
    /// in it.
    ///
    /// Two things still have to happen before the process exits, because they
    /// are not the checkout's to carry and nothing else will ever bank them
    /// for this turn. The lane's spend is banked, because an interrupted lane
    /// spent exactly as many tokens as one that finished and they would
    /// otherwise leave the accounting entirely. And the launch counter is
    /// forgiven — see [`Task::launch_landed`] — because the launch it counted
    /// is the one still running: without this, a task whose lane survives the
    /// stop is `blocked` the moment the next run starts, on a launch that
    /// never actually failed.
    ///
    /// **A task parked in front of a person** — `paused`, or `blocked` with
    /// nobody staffed to answer it — **is skipped outright.** Nothing about it
    /// is running, so there is nothing here to bank or forgive; see
    /// [`Dispatcher::parked_for_a_person`].
    pub fn sweep_on_stop(&mut self, report: &mut Report) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }

        let mut tasks = self.repo.tasks()?;
        let mut left_standing = 0usize;

        for task in &mut tasks {
            if self.parked_for_a_person(task) {
                continue;
            }
            if task.front.workspace_id.is_none() && task.front.worktree_path.is_none() {
                continue;
            }

            // `record_usage` reads the task for the plan and the step's own
            // verdict, and the step is the stage this task is still sitting on.
            let step_id = task.stage().to_string();
            let name = lane_name(&step_id, task.id());
            // Read rather than removed: the lane this record tracks is left
            // running, so its bookkeeping — `notified`, `reminded_at` and the
            // rest of what the reminder loop reads — has to survive into the
            // next dispatcher's own `self.lanes`, the same as everything
            // `save_lane_records` below carries across for a lane nobody
            // touched at all this pass. `readopted` covers the one case a
            // plain lookup cannot: this stop's own dispatcher may itself be a
            // restart of one that died mid-pass, in which case `self.lanes`
            // never had this lane's record to begin with — see
            // `LaneRecord::readopted`. `record_usage` is a safe no-op either
            // way, on the blank session a lane the ledger has genuinely never
            // heard from still gets.
            let ledger = self.ledger();
            let record = self
                .lanes
                .get(&name)
                .cloned()
                .unwrap_or_else(|| LaneRecord::readopted(&name, now_secs(), &ledger));
            let pipeline = self
                .pipelines
                .for_task(task)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            self.record_usage(&record, task.id(), &step_id, Some(task), &pipeline);

            task.launch_landed();
            task.save()?;
            left_standing += 1;
        }

        // Whatever this pass banked, or any other lane's own bookkeeping —
        // untouched above — is what the next dispatcher reads. Nothing else
        // writes this file on this path: a stop happens between passes, and
        // `pass` is what ordinarily saves it.
        save_lane_records(self.repo, &self.lanes)?;

        if left_standing > 0 {
            report.actions.push(format!(
                "stopping: {left_standing} lane(s) left standing — their worktrees, panes \
                 and agents are where they were"
            ));
        }

        Ok(())
    }
}

/// The task that recorded `branch` as its own `branch:` — matched exactly,
/// not by taking the branch name apart. `queue add` stamps `task/<id>`, or
/// `task/<slug>-<id>` when `issue_tracking.key_in_names` prefixed it, and the
/// slug is opaque, so its length cannot be read back out of the branch. This
/// walks candidate ids by dropping one leading `<segment>-` at a time, but
/// accepts a candidate only when the task file it names records this exact
/// branch — so `task/proj-old-auth-01` resolves to `auth-01` (slug
/// `proj-old`) and never to a real `old-auth-01` whose own branch is
/// something else.
///
/// The queue is searched before the archive, so a task still in flight
/// returns its live record. A task file that records no `branch:` at all — a
/// hand-dropped one that never passed `queue add` — matches only the bare
/// `task/<id>` its id implies, never a prefixed branch. `None` when no task
/// on disk claims the branch — its task swept from the archive by
/// `retention.days`, say — which the sweep then leaves alone.
fn task_for_branch(repo: &crate::repo::Repo, branch: &str) -> Option<Task> {
    let mut candidate = branch.strip_prefix("task/")?;
    loop {
        for dir in [repo.queue_dir(), repo.archive_dir()] {
            if let Ok(task) = Task::load(&dir.join(format!("{candidate}.md"))) {
                let claims = match task.front.branch.as_deref() {
                    Some(recorded) => recorded == branch,
                    None => branch == crate::task::default_branch(candidate),
                };
                if claims {
                    return Some(task);
                }
            }
        }
        match candidate.split_once('-') {
            Some((_, tail)) if !tail.is_empty() => candidate = tail,
            _ => return None,
        }
    }
}
