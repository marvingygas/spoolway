---
name: spoolway-tasks
description: Cut an already-approved shape into task documents in the pending directory — gather every pipeline's own window, route each subject to one before sizing it, offer the shape, write the documents, verify the chain and its `last:` step, and prove the set with `spoolway task contract`. Invoked from another skill's own step, such as spoolway-plan's step 7 once a plan is approved, or run directly once a shape is already agreed.
---

# spoolway-tasks

Turn an agreed shape into the task documents that carry it — the one procedure every caller
that cuts a breakdown shares, so a second caller does not drift from the first the day either
is fixed. Unlike the skills that call it, this one carries no invocation restriction in its
frontmatter: it has to be reachable from inside another skill's own procedure, not only from a
person's own prompt.

A task is a document, and nothing else. It goes in the pending directory —
`~/.spoolway/<project>/pending/<task-id>.md`, `<project>` being the basename of the repo
root — where `spoolway queue` reads it. Nothing is appended to whatever page or record the
shape came from.

**Refuse to re-cut a plan whose tasks already exist.** `ls ~/.spoolway/<project>/pending/`
and `spoolway group list`: a document still pending, or a task of this group still in the
queue, means the breakdown is already out there. A further change is a new plan with a group
of its own, or a revision above the tasks, not a re-cut.

## Procedure

When this is invoked directly against an issue — not through **spoolway-plan**, which already
read one at its own step 1 — run `spoolway issue show <ref>` first, and decompose from its
title, body, labels and comments together: a correction or a scope cut often lives in a
comment, not the body.

1. **Gather, before anything is sized.** Three commands, and nothing is opened past them:

   - `spoolway task contract` — the project's default pipeline, and per pipeline its
     `id_budget`, its `gate_at` steps, and its skeleton `body`.
   - `spoolway pipeline show` — every pipeline's own `description:`, the model on every agent
     step, and which steps are `last-of-chain`.
   - `spoolway models` — the `WINDOW` per model.

   Cross the last two yourself: a pipeline's own window is the smallest window among the
   models on its own agent steps, and a model `spoolway models` cannot resolve a window for
   is still the small case — cut for the window you can prove, not the one you hope for. No
   file is opened here.

2. **Route each subject to a pipeline, then decompose against that pipeline's own window.**
   Read the pipelines before choosing task boundaries. Let their purpose, steps, and context
   windows shape the split instead of fitting pipelines onto an already-cut list.

   Each task: **independently completable**, one model, one worktree, no
   coordination; **whole enough that one lane holds the change at once**, split by *subject*
   not size; **small in criteria too**, five bullets or split; **ordered**, chained with
   `depends_on` (a **join** — one task depending on two — rebases onto only one parent,
   shipping silently missing the other's work); **disjoint** in files touched, so an overlap
   is at worst a merge conflict. **A genuine fan is deliberate, not a default**: tasks with
   nothing to say about each other's diff may skip the chain, but mark both `parallel: true`
   when they do — still a mistake to share a `touches` glob.

   **Route each subject before sizing it, yourself.** Match each subject against step 1's
   `pipeline show` output — a bug found mid-plan wants `bugfix`, feature work beside it wants
   `default`, and so on by what each pipeline's own `description:` says it is for. Nobody signs
   off on this pick in isolation: it is not asked as its own question, because it reaches the
   person only once, on the split ballot below, where every candidate already shows it.

   **Size for the lane that implements it, never for a person.** A lane is an agent in a
   fresh worktree with a context window, not a developer with an afternoon — so "a session"
   and "a day's work" are the wrong units and do not belong in this judgement at all. Judge
   instead by what one lane must hold: the files it reads to understand the change, the
   files it changes, and whether the whole thing is one subject.

   **Which way to lean is the routed pipeline's own window to decide, not yours** — the
   number step 1 crossed for it, not one figure for the whole batch, so two subjects on two
   pipelines may lean two different ways in the same breakdown.

   - **A window in the millions leans bigger.** Every extra task pays for another lane to
     read the codebase, the prompt and the task from scratch before it writes a line, and
     runs the pipeline's `review`, `e2e` and `document` steps again on top. The implementing
     lane that a split relieves almost always had room to spare. Torn between two splits,
     take the larger tasks and the smaller count — cutting too small is the mistake that
     actually happens at this scale.
   - **A window in the hundreds of thousands cuts smaller.** A local model has to hold the
     files it reads, the diff it writes and its own reasoning in a window an order of
     magnitude tighter, and it has no compaction worth the name — a task that overflows does
     not slow down, it forgets the contract it was given and fails the step. Torn between two
     splits, take the smaller tasks and the larger count, and let the criteria fall well
     under the five bullets rather than up against them.

   **Offer the shape, as one printed ballot.** Settle on a recommended count first, then put
   **that count and the four below it** on the ballot, floored at 1 — a recommendation of 6
   names 2, 3, 4, 5 and 6; a recommendation of 3 names 1, 2 and 3. Name every member in the
   question text — `Split this shape: 2, 3, 4, 5 or 6 tasks — or generate a pipeline` — and
   give every count its own lettered line, largest first, the recommended one marked as such.
   A printed ballot has no option limit, so nothing is left for the person to type out; the
   last option is always "Generate a pipeline for this plan" (`spoolway pipeline gen --plan
   <path>`, one line, session ends with nothing cut). **pi has no dialog tool, so the ballot
   is printed and the turn ends there** — the answer arrives as the person's next prompt. Put
   size and the routed pipeline's own name against every task name on every candidate, and
   every pipeline named there carries its own `description:` verbatim in that option's own
   text — a person approves the routing by picking the count, not by a question of its own, so
   this is the one place the sentence has to be. No pipeline reaches a document without having
   appeared on this ballot first.

3. **Write one document per task**, with **Write**, at
   `~/.spoolway/<project>/pending/<task-id>.md`. Frontmatter first, then the body in the shape
   step 1's contract already printed for this task's own pipeline — the `body` field, byte-
   identical to `.spoolway/templates/tasks/<pipeline>.md`:

   - `id` — the task id, and the file's own stem. **Keep it short enough to become a lane
     name:** a lane is called `<id> · <step>` and stops at 34 bytes. Measure against the
     `id_budget` step 1's contract already printed for this task's own pipeline — `bugfix`'s
     is 15 — rather than working the arithmetic again.
   - `title` — a Conventional Commits line: a type, the area of code in parentheses, a colon,
     and one short present-tense sentence. `feat(queue): add a --dry-run flag`. The type is
     `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci` or `chore`; the
     parentheses come off when no single area fits. It is used verbatim — as the squashed
     commit's subject, as the pull request's title where nothing else sets one, and as the line
     the queue screen draws under the task — so the sentence still has to say what the task is
     for on its own.
   - `group` — **the same string on every document of this breakdown**, read verbatim and
     never a path. It is what makes them one row on the queue screen, one selection, and one
     tab at run time. The plan's own slug is the obvious value.
   - `source` — the issue's own URL, from `spoolway issue show`, when this breakdown started
     at one; the calling page's own absolute path otherwise, where one exists. The screen's
     `o` key opens a `source:` that is a URL; a path is carried for the reader rather than
     opened.
   - `plan` — the calling page's own absolute path, when there is both an issue *and* a page
     — an issue read straight into tasks with no page carries no `plan:` at all, and neither
     does a page with no issue behind it, since `source:` already carries its path there.
   - `touches`, `depends_on`, `pipeline`, and `parallel: true` on each half of a deliberate
     fan.

   The body's `## Mockup` copies in the steps that task owns, from the calling record's own
   Mockup, only when the task changes something a person opens. Never paste a document's
   contents into the body — name the path instead. Include an end-to-end coverage line naming
   a test reached, or "none, and why". Never write an instruction to *run* anything — a lane
   reading it will try to.

4. **Verify the shape.** Walk the `depends_on` of the documents you just wrote: a line —
   exactly one task with no dependency, one with no dependent — except where two are
   `parallel: true`, a chosen gap. **A join has to be fixed before you go on**: re-run step 2
   rather than patch ids after the fact.

   Overlapping `touches` globs are not yours to resolve here any more. The queue screen walks
   every genuine collision with the person before it writes anything, and that walk is where
   an ordering gets chosen. Say the pair out loud to the caller if you left one knowingly; do
   not invent a section for it.

   **Then check the top of the chain carries its own `last:` step.** The task with no
   dependent — the one nothing else in this breakdown depends on, or the only task where
   there is no chain at all — is the only one whose run of the pipeline ever reaches a step
   marked `last-of-chain` in step 1's `pipeline show`. When that task's own routed pipeline
   carries none, the whole group loses that step silently: nobody after it ever runs it, since
   every other task in the chain walks straight past. Re-route that one task — to the
   project's default pipeline when it carries a `last-of-chain` step, otherwise to the first
   pipeline from step 1's gathering that does — and say the swap out loud to the caller
   instead of just writing a different `pipeline:` into its document.

5. **Prove the documents.** `spoolway task contract --from ~/.spoolway/<project>/pending` —
   the same validation `queue add --from` runs, stopping short of the save. Fix what it
   reports, re-run until it passes, never mention the loop to the human.

6. **Say one line**, once: `<n> tasks written to ~/.spoolway/<project>/pending. Open the
   queue screen (\`spoolway queue\`) to send them.` Never summarise the tasks themselves.

## Guardrails

- Never invent a goal, criterion or reference the shape it came from doesn't support, and
  never leave a document as the unfilled skeleton.
- Never write a document anywhere but the pending directory, and never write anything else
  there — it is a queue of task documents, not a scratch directory.
- Never call `spoolway queue add` or start a dispatcher here — writing a document is not
  queueing it; that is the screen's job, done separately by a human.
