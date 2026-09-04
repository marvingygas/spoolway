---
domain: collaboration
covers: ["src/handover.rs"]
---

# Collaboration

How in-flight work moves between machines and between people, without anybody editing a file
by hand and without two dispatchers ever running the same task.

## Why task files never enter the tree

Task files are runtime state, not code. They change on every transition, and a worktree
carrying a copy of every one of them would shadow the canonical queue. So they are never
committed.

Instead, `spoolway handover` mirrors them to a ref only your machine writes:

```
refs/spoolway/<your git user.email>/queue
```

Alongside it, under the same `refs/spoolway/<your git user.email>/` namespace, sit the task
branches those files name (`/branch/<branch>`) and a snapshot of anything a lane has not
committed yet (`/wip/<branch>`). The snapshot is taken with `git add -A` through an index of
its own, so it carries untracked files too, not only modified ones — everything except what
`.gitignore` excludes. Refs of their own rather than more of the queue ref, because git
refuses a ref that is a prefix of another.

**None of your project's code is on that ref.** The code lives on ordinary branches, pushed by
the `handover` step, exactly as before.

One writer per ref means there is nothing to merge and no ownership to negotiate.

## Taking work from a colleague

```
spoolway adopt --from you@corp.com --group payments
spoolway adopt --from you@corp.com login sessions
spoolway adopt --from you@corp.com                  # everything they have
```

One command, run entirely from your side: the person handing over does not have to be there,
or to have done anything.

It is a **move**, not a copy:

- The task files leave their mirror and arrive in your queue.
- Their next `spoolway handover` notices and lets go of them locally — except a task whose lane
  is still running on that machine, which it keeps and re-mirrors. Yanking the file would break
  that lane's own `spoolway report`. The task stays with the machine doing the work, and you
  are left holding a duplicate to delete.
- The task branches are fetched and created here — **with the work on them**, which a worktree
  cut for a branch that exists only as a remote-tracking ref would silently not have.

**Placement never travels.** Which worktree, which pane, which workspace — that is the one part
of a task file that is about a machine rather than about the work, and it is cut fresh on
whichever machine picks the task up.

Nothing has to be edited by hand. In the ordinary case a task file exists in exactly one
queue, so two dispatchers never run one task. Adopting a task whose lane is still live
elsewhere is the exception above: for as long as the duplicate sits in your queue unresolved,
both machines could start it.

## Handing work over deliberately

```
spoolway handover
spoolway handover --group payments
spoolway handover --group payments --reset-unpublished
```

Mirroring is deliberate, not automatic: nothing is pushed until you run this. Run it before
handing work over, and run it again after a colleague has adopted — that second run is when
this machine notices what they took and lets go of it locally.

`--reset-unpublished` sends tasks that never reached `handover` back to the start of their
pipeline. Their commits are on your machine and nowhere else, so a colleague cannot have
them — and a task file's goal and acceptance criteria are a better thing to inherit than a
half-finished worktree. It also clears the task's `branch`, `cut_from` and `base_commit`, so
whoever adopts it cuts a fresh worktree from `base` rather than landing back on the abandoned
attempt's branch and commits.

Which tasks those are is asked of the remote — `git ls-remote` on the task's own branch —
rather than inferred from the step it is sitting on: step ids are a project's to rename, and a
list of them here would silently stop matching the day somebody did.

## Several groups, one queue

The queue is the project's, not a worktree's. Every spoolway command finds the project
through git rather than by walking up the filesystem, so a group in a worktree of its own
still shares:

- **one queue**, so a task queued from any worktree is picked up by the next pass
- **one dispatcher**, serving every group's tasks from that one queue
- **one usage ledger**, so cost is per project rather than per branch

A group whose branch has diverged holds back its own dependents and nobody else's.

## What is safe to interrupt

All of it. The dispatcher remembers nothing between passes and reconciles from the task files
and the live lane list, so stopping it, moving machines, adopting somebody's work and
starting it again loses nothing that was written down.

What is *not* written down is a lane's uncommitted work in progress — which is exactly what
the mirror snapshots, and exactly what `--reset-unpublished` chooses to discard when it would
be misleading to inherit.
