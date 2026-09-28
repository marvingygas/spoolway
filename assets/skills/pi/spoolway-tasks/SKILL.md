---
name: spoolway-tasks
description: Cut an already-approved shape into tasks in the pending directory — gather each pipeline's own contract, route each subject to one before sizing it, offer the shape, write the tasks, verify the chain and its `last:` step, and prove the set with `spoolway task contract`. Invoked from another skill's own step, such as spoolway-plan's step 7 once a plan is approved, or run directly once a shape is already agreed.
---

# spoolway-tasks

Turn an agreed shape into the tasks that carry it — the one procedure every caller
that cuts a breakdown shares, so a second caller does not drift from the first the day either
is fixed. Unlike the skills that call it, this one carries no invocation restriction in its
frontmatter: it has to be reachable from inside another skill's own procedure, not only from a
person's own prompt.

A task is a markdown file, and nothing else. It goes in the pending directory —
`~/.spoolway/<project>/pending/<task-id>.md`, `<project>` being the basename of the repo
root — where `spoolway queue` reads it. Nothing is appended to whatever page or record the
shape came from.

**Refuse to re-cut a plan whose tasks already exist.** `ls ~/.spoolway/<project>/pending/`
and `spoolway group list`: a task still pending, or a task of this group still in the
queue, means the breakdown is already out there. A further change is a new plan with a group
of its own, or a revision above the tasks, not a re-cut.

## Procedure

When this is invoked directly against an issue — not through **spoolway-plan**, which already
read one at its own step 1 — run `spoolway issue show <ref>` first, and decompose from its
title, body, labels and comments together: a correction or a scope cut often lives in a
comment, not the body.

When this is invoked directly against a plan page — its path, not through **spoolway-plan**'s
own session, which already holds the shape it argued — read only the page's machine copy of
itself, never the markup around it: `sed -n '/id="plan"/,/<\/script>/p' <path>`. Where a page
predates that block and the command comes back empty, say so to the caller and fall back to
reading the file whole.

1. **Gather, before anything is sized.** One command, and nothing is opened past it:
   `spoolway task contract` — the `base` this checkout has out, every pipeline this project
   defines, its own `sizing` guidance, and per pipeline its `description:`,
   `longest_agent_step`, `gate_at` steps, `last_of_chain`, and skeleton `body`.

   **The branch you are on is what the work builds on.** `base` is the branch this checkout
   has out, and every task takes it unless a note on the ballot changes it. When `base` comes
   back `null`, the checkout is detached: stop there, write nothing, and tell the caller to
   check out the branch this work builds on and run this again.

   **A project with no pipelines is not a dead end.** Where this reports `no pipelines
   defined`, install the shipped ones yourself: a bare `spoolway init --yes` — `--provider`
   takes only `claude` or `codex`, not `pi`, so this is the same mechanical fix
   **spoolway-config**'s own pi copy reaches for when it repairs a project. Re-runnable, and it
   writes only what is absent, so it restores just the missing pipelines and prompts and leaves
   everything else in the project untouched. Then re-run `spoolway task contract` and carry on
   with what it now reports, without asking the person first — say one line about it, `Installed
   the two shipped pipelines; routing against them.`, ahead of the ballot in step 2.

2. **Route each subject to a pipeline, then decompose against that pipeline's own shape.**
   Read the pipelines before choosing task boundaries. Let their purpose and steps shape the
   split instead of fitting pipelines onto an already-cut list.

   Each task: **independently completable**, one model, one worktree, no
   coordination; **whole enough that one lane holds the change at once**, split by *subject*
   not size; **small in criteria too**, five bullets or split; **ordered**, chained with
   `depends_on` (a **join** — one task depending on two — rebases onto only one parent,
   shipping silently missing the other's work). Whether two tasks may run side by side is
   judged from what each one actually changes, never from a shared file — a shared file
   alone is no reason to chain them. **A genuine fan is deliberate, not a default**: tasks
   whose changes do not step on each other may skip the chain, but mark both `parallel: true`
   when they do.

   **Route each subject before sizing it, yourself.** Match each subject against step 1's
   contract — a bug found mid-plan wants `bugfix`, feature work beside it wants `default`,
   and so on by what each pipeline's own `description:` says it is for. Nobody signs off on
   this pick in isolation: it is not asked as its own question, because it reaches the person
   only once, on the split ballot below, where every candidate already shows it.

   **Size for the lane that implements it, never for a person.** A lane is an agent in a
   fresh worktree, not a developer with an afternoon — so "a session" and "a day's work" are
   the wrong units and do not belong in this judgement at all. Judge instead by what one lane
   must hold: the files it reads to understand the change, the files it changes, and whether
   the whole thing is one subject. No arithmetic: step 1's own `sizing` field says it plainly
   — cut a reasonable number of tasks for the shape at hand, judged by subject, with each
   task's criteria kept under five bullets or split again.

   **Offer the shape, as one printed ballot.** Settle on a recommended count first, then put
   **that count and the three below it** on the ballot, floored at 1 — a recommendation of 6
   names 3, 4, 5 and 6; a recommendation of 3 names 1, 2 and 3, floored rather than padded back
   up to four. Open with the line that names every member — `Split this shape: 3, 4, 5 or 6
   tasks` — then give every count its own lettered section, largest first, the recommended one
   marked as such, and say that the person answers with a letter.

   **Print the ballot and end the turn there.** Do not reach for a dialog tool: pi has none
   of its own, and what a given install's plugins offer is not something this skill can count
   on. The answer arrives as the person's next prompt, so do not go on to write anything
   until it does.

   **Every candidate is written in this one layout, and never in another:**

   ```
   1  <task-id>                  <size>   <pipeline>
      base  <branch>
      What this task is, in one or two plain sentences.

   2  <task-id>                  <size>   <pipeline>
      What this task is, in one or two plain sentences.
   ```

   Number the tasks from 1 in `depends_on` order, so the chain reads down the page.
   `<task-id>` is the id the task will carry, not a prose title. `<size>` is `small`,
   `medium` or `large` — the sizing assumption you just made, put where a person can see it,
   so a `large` where they expected two tasks is something they can turn down by picking a
   different count. Line the three columns up with spaces, pipeline last, and wrap the
   sentences under each task at around 48 characters, because a narrow terminal is what reads
   them.

   **A task with no `depends_on` starts a chain, and carries a `base` line** — step 1's `base`,
   until a note changes it. A task that depends on another shares that chain's base and shows
   no `base` line of its own. Never propose any other base yourself: not a pending pull
   request, not the default branch.

   **A note on the answer changes a chain's base.** The person may add a note after their
   letter, such as `1 from #412, cart-empty from main`: a task's number or id, `from`, then a
   branch or a pull request number. Each part sets the base of the chain that task is in. A
   branch is taken as written. A `#<n>` is resolved with `gh pr view <n> --json
   headRefName,state,isCrossRepository`, and its `headRefName` is the base.

   Then print the ballot once more, with the resolved bases drawn, and end the turn there
   again. Open it with the notes line, `notes: 1 from #412, cart-empty from main`, and draw
   each task on its one row, with every chain's `base` line under its first task and no
   sentences:

   ```
   1  cart-totals      medium   default
      base  task/gh-412-checkout
   2  cart-discounts   small    default
   3  cart-empty       small    default
      base  main
   ```

   The answer to that one stands. Bring the ballot back with the problem named, and nothing
   changed, when a note names no task on the ballot, names a number `gh pr view` finds no
   pull request for, names a pull request from a fork (`isCrossRepository` is true — its
   branch is not in this repository), or names one that is not open (`state` is not `OPEN`).

3. **Write one file per task**, with **Write**, at
   `~/.spoolway/<project>/pending/<task-id>.md`. Frontmatter first, then the body in the shape
   step 1's contract already printed for this task's own pipeline — same headings, same order
   as the `body` field's `.spoolway/templates/tasks/<pipeline>.md`, with every `[[bracketed]]`
   placeholder replaced by this task's own answer, never carried through unfilled. A heading
   with nothing of its own to say for this task (an optional `## Mockup`, say) is dropped
   rather than left standing in on the placeholder's own words. The same goes for a heading's
   un-bracketed guidance paragraph — "Three to five facts, one line each. …" under `## Context`,
   "Out of scope. Doing any of these is a review failure, not a bonus." under `## Non-goals`,
   "Read these before you start. …" under `## References`, and the like: that prose is an
   instruction to you, not words for the finished task, and it is replaced by this task's
   own answer or dropped with the rest of an unused heading — never left standing as if the
   task itself said it:

   - `id` — the task id, and the file's own stem. Lowercase letters, digits and hyphens only,
     starting with a letter — the same path-safe rule every id on this project follows. No
     length budget: a lane too long for the multiplexer's own name limit gets a short internal
     alias instead, so an id is sized for readability, not for fitting a lane name.
   - `title` — a Conventional Commits line: a type, the area of code in parentheses, a colon,
     and one short present-tense sentence. `feat(queue): add a --dry-run flag`. The type is
     `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `build`, `ci` or `chore`; the
     parentheses come off when no single area fits. It is used verbatim — as the squashed
     commit's subject, as the pull request's title where nothing else sets one, and as the line
     the queue screen draws under the task — so the sentence still has to say what the task is
     for on its own.
   - `group` — **the same string on every task of this breakdown**, read verbatim and
     never a path. It is what makes them one row on the queue screen, one selection, and one
     tab at run time. The plan's own slug is the obvious value — the slug alone, with the
     `<YYYY-MM-DD>-` of the plan file's own name off it. A group carrying a date pins the
     whole breakdown to the morning it was cut.
   - `source` — the issue's own URL, from `spoolway issue show`, when this breakdown started
     at one; the calling page's own absolute path otherwise, where one exists. The screen's
     `o` key opens a `source:` that is a URL; a path is carried for the reader rather than
     opened.
   - `plan` — the calling page's own absolute path, when there is both an issue *and* a page
     — an issue read straight into tasks with no page carries no `plan:` at all, and neither
     does a page with no issue behind it, since `source:` already carries its path there.
   - `base` — on every task, a dependent included: its chain's base from step 2, which is
     step 1's `base` unless a note changed it. A dependency and its dependent must share one.
   - `depends_on`, `pipeline`, and `parallel: true` on each half of a deliberate fan.

   **Link a source; never redraw it.** Whatever the shape came from as a file — the calling
   page, a screenshot, anything the person named or pasted — is linked from the task, never
   copied or translated into Markdown. A lane reads nothing outside its worktree and the
   project home, so a file anywhere else is first copied into
   `~/.spoolway/<project>/plans/<group>/` and linked there.

   The body's `## Mockup`, only when the task changes something a person opens, starts on
   this line, word for word:

       Open each link and build what it draws. If it cannot be built as drawn, say so in
       your report rather than improvising something near it.

   then one line per step or file the task builds: the page's absolute path and the step's
   own id, `<page>#m-<slug>`, and the step's heading. `## References` links each decision
   record the task implements the same way, `<page>#d-<slug>`. The ids are on the page:
   `grep -n 'id="[dm]-' <page>`. Panels are written into the body only when no file draws
   them — a shape settled out loud and drawn only in the chat. Never paste a task's
   contents into the body — name the path instead. Include an end-to-end coverage line naming
   a test reached, or "none, and why". Never write an instruction to *run* anything — a lane
   reading it will try to.

   **Write every line of it in plain English.** One thing per sentence, in the shortest words
   that carry it — no story, no build-up, no buzzwords, and no reaching verbs: "reads",
   "writes", "moves", never "orchestrates", "leverages", "unlocks". Jargon costs more here
   than on a plan page, because the reader is a lane holding nothing but this task: name
   a thing the way the code names it, and spell out anything the repo has not already named.
   Complete is the bar rather than terse — but a sentence that adds no fact is still cut.

4. **Verify the shape.** Walk the `depends_on` of the tasks you just wrote: a line —
   exactly one task with no dependency, one with no dependent — except where two are
   `parallel: true`, a chosen gap. **A join has to be fixed before you go on**: re-run step 2
   rather than patch ids after the fact.

   Whether two tasks are safe to run side by side is yours to judge from what each one
   changes, not from a shared file — spoolway reports no overlap of its own, and `enter`
   writes both tasks straight through with no `depends_on` invented. So say the pair out
   loud to the caller if you judged them safe beside each other on a shared file, and never
   tell them something downstream will check it; do not invent a section for it.

   **Then check the top of the chain carries its own `last:` step.** The task with no
   dependent — the one nothing else in this breakdown depends on, or the only task where
   there is no chain at all — is the only one whose run of the pipeline ever reaches a step
   marked `last-of-chain` in step 1's contract. When that task's own routed pipeline
   carries none, the whole group loses that step silently: nobody after it ever runs it, since
   every other task in the chain walks straight past. Re-route that one task to the first
   pipeline from step 1's gathering that carries a `last-of-chain` step — there is no project
   default to prefer over the rest — and say the swap out loud to the caller instead of just
   writing a different `pipeline:` into its task.

5. **Prove the tasks.** `spoolway task contract --from ~/.spoolway/<project>/pending` —
   the same validation `queue add --from` runs, stopping short of the save. Fix what it
   reports, re-run until it passes, never mention the loop to the human.

6. **Say one line**, once: `<n> tasks written to ~/.spoolway/<project>/pending. Open the
   board to send them.` Never summarise the tasks themselves.

## Guardrails

- Never invent a goal, criterion or reference the shape it came from doesn't support, and
  never leave a task as the unfilled skeleton.
- Never write a task anywhere but the pending directory, and never write anything else
  there — it is a queue of tasks, not a scratch directory. The one other place this skill
  writes is `~/.spoolway/<project>/plans/<group>/`, and only a source file copied in
  to be linked.
- Never redraw a source in a task. A figure, a panel or a screenshot that exists as a file
  is linked; only what exists nowhere else is written out.
- Never call `spoolway queue add` or start a dispatcher here — writing a task is not
  queueing it; that is the screen's job, done separately by a human.
