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
`<home>/pending/<task-id>.md` — where `spoolway queue` reads it. Nothing is appended to
whatever page or record the shape came from.

**`<home>` is this project's own home, `~/.spoolway/<label>-<id>/`, and never a path built from
the repo's name.** A folder named after the repo alone is one spoolway never reads. Take it off
the contract instead: `spoolway task contract` prints the pending directory as `output.dir`,
and `<home>` is the directory above it.

**Refuse to re-cut a plan whose tasks already exist.** `ls <home>/pending/`
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
   defined`, install the shipped ones yourself: `spoolway init --provider claude --yes` —
   re-runnable, and it writes only what is absent, so it restores just the missing pipelines
   and prompts and leaves everything else in the project untouched. Then re-run `spoolway
   task contract` and carry on with what it now reports, without asking the person first —
   say one line about it,
   `Installed the two shipped pipelines; routing against them.`, ahead of the ballot in step 2.

2. **Settle the groups first, with one AskUserQuestion, before anything is sized.**

   **A group is always one chain**, so work that would not chain goes in a group of its own
   instead of a fan inside one. Cut groups so that no two of them touch the same files: two
   pieces of work that share no changes run side by side, each in its own group. Where two
   pieces would conflict, keep them in one group, or, when one plainly must land after the
   other's own work, let the later piece's group stack on the earlier one instead, naming its
   last task in its own first task's `depends_on`. A group may stack on at most one other
   group, since two would be a join. Lean towards one group — most plans need it, and a split
   group costs an extra `last:` run. A split group is named `<plan-slug>-<part>`, `<part>` a
   short word for what it holds, never a number.

   Put `1 group (Recommended)` first, always, its own `preview` drawing the one line the plan
   stays as. Add one option per split worth offering, each carrying this layout in its
   `preview`, every group its own header line and its own sentence beneath it, one blank line
   between groups, in dependency order:

   ```
   <group-id>                    base  <branch>
      One sentence on what the group holds.

   <group-id>               after <group-id>
      One sentence on what the group holds.
   ```

   A group that stacks on no other carries a `base` line — the branch from step 1, unless the
   plan argues another. A group stacked on another carries `after <group-id>` instead, never a
   `base` line of its own. Ask `Split this into groups?`. When the skill sees no split worth
   making, skip the ballot: say `Keeping one group.` and go straight on, with every task in one
   group.

   **Then, for each group, in dependency order, route each subject inside it to a pipeline and
   decompose against that pipeline's own shape.** Read the pipelines before choosing task
   boundaries. Let their purpose and steps shape the split instead of fitting pipelines onto
   an already-cut list.

   Each task: **independently completable**, one model, one worktree, no
   coordination; **whole enough that one lane holds the change at once**, split by *subject*
   first, then by size; **small in criteria too**, five bullets or split; **ordered**, chained with
   `depends_on` (a **join** — one task depending on two — rebases onto only one parent,
   shipping silently missing the other's work). Whether two tasks may run side by side is
   judged from what each one actually changes, never from a shared file — a shared file
   alone is no reason to chain them; work that would not chain is a group of its own, decided
   above, not a fan inside this one.

   **Route each subject before sizing it, yourself.** Match each subject against step 1's
   contract — a bug found mid-plan wants `bugfix`, feature work beside it wants `default`,
   and so on by what each pipeline's own `description:` says it is for. Nobody signs off on
   this pick in isolation: it is not asked as its own question, because it reaches the person
   only once, on the split ballot below, where every candidate already shows it.

   **Size for the lane that implements it, never for a person.** A lane is an agent in a
   fresh worktree, not a developer with an afternoon — so "a session" and "a day's work" are
   the wrong units and do not belong in this judgement at all. Judge instead by what one lane
   must hold: the files it reads to understand the change, the files it changes, and how many
   separate pieces it builds. A subject that builds several pieces that each work on their own
   is more than one lane holds: cut it along those pieces, in the same chain, and route each
   piece on its own. Keep a group's tasks roughly even in size: cut again where one is far
   larger than its siblings, and fold a piece into its neighbour where it is far smaller. No
   arithmetic: step 1's own `sizing` field says it plainly — cut a reasonable number of tasks
   for the shape at hand, judged by subject first and size second, with each task's criteria
   kept under five bullets or split again.

   **Offer this group's shape, with one AskUserQuestion.** Settle on a recommended count
   first, then put **that count and the three below it** on the ballot, floored at 1 — a
   recommendation of 6 names 3, 4, 5 and 6; a recommendation of 3 names 1, 2 and 3, floored
   rather than padded back up to four. Name every member and the group in the question text —
   `Split <group>: 3, 4, 5 or 6 tasks` — and put all four counts on the ballot itself, largest
   first and marked recommended; AskUserQuestion's four option slots are exactly enough now
   that none of them goes to a generation option.

   **Every candidate is written in this one layout, and never in another:**

   ```
   1  <task-id>                  <size>   <pipeline>
      base    <branch>
      labels  <label>, <label>
      What this task is, in one or two plain sentences.

   2  <task-id>                  <size>   <pipeline>
      What this task is, in one or two plain sentences.
   ```

   Number the tasks from 1 in `depends_on` order, so the chain reads down the page.
   `<task-id>` is the id the task will carry, not a prose title. `<size>` is `small`,
   `medium` or `large` — the sizing assumption you just made, put where a person can see it,
   so a `large` where they expected two tasks is something they can turn down by picking a
   different count. Line the three columns up with spaces, pipeline last, and wrap the
   sentences under each task at around 48 characters, because the preview box they are read
   in is narrow.

   **A task with no `depends_on` starts a chain, and carries a `base` line** — step 1's `base`,
   until a note changes it or the split ballot above recommended a pull request and the person
   picked the option that shows it. A task that depends on another shares that chain's base
   and shows no `base` line of its own.

   **Recommend an open, same-repository pull request as a chain's base only when that chain's
   work needs the pull request's change**, and name the reason in one line under the `base`
   line. Check it the way the note mechanism below already does: `gh pr view <n> --json
   headRefName,state,isCrossRepository`, refusing one that is not open, is from a fork
   (`isCrossRepository` is true), or that the command finds nothing for. Where a chain clears
   this, draw it on the split ballot above, not a ballot of its own: the recommended option's
   `base` line reads `#<n> (<headRefName>)`, with the reason beneath it, and one of the other
   options on that same ballot — its task breakdown otherwise unchanged — keeps that chain's
   `base` line at step 1's own `<branch>`, so declining costs one pick rather than a note. When
   the floor leaves only one count on the ballot, add a second option for that same count,
   differing only in this `base` line. Never recommend a pull request a chain's work does not
   need, a closed one, or one from a fork.

   **A task carries a `labels` line when it proposes any.** Comma-separated plain words, drawn
   from the plan that argued this shape or from the source issue's own labels — `spoolway issue
   show`'s own `labels` array, when this breakdown started at one — never invented fresh here.
   Dropped for a task that proposes none, the same way a dependent's own `base` line is.

   **The block is the option's `preview`, never its `description`.** `preview` is the only
   field that renders it as written: a monospace box that keeps the newlines and holds the
   three columns in line. A `description` is prose — it reflows, and the newlines come back
   as stray glyphs that run every candidate together into one paragraph nobody can read a
   count out of. So each option carries the block in `preview`, and in `description` one
   plain sentence saying what that count trades away — never a second copy of the layout.
   Previews need `multiSelect: false`, which this ballot already is.

   **A note on the answer changes a chain's base.** The person may add a note to the option
   they pick, such as `1 from #412, cart-empty from main`: a task's number or id, `from`, then
   a branch or a pull request number. Each part sets the base of the chain that task is in. A
   branch is taken as written. A `#<n>` is resolved with `gh pr view <n> --json
   headRefName,state,isCrossRepository`, and its `headRefName` is the base.

   Then ask the ballot once more, with the resolved bases drawn. Open the question text with
   the notes line, `notes: 1 from #412, cart-empty from main`, and draw each task on its one
   row, with every chain's `base` line under its first task and no sentences:

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
   `<home>/pending/<task-id>.md`. Frontmatter first, then the body in the shape
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
   - `group` — **the same string on every task of its own group**, read verbatim and never a
     path. It is what makes them one row on the queue screen, one selection, and one tab at
     run time. The plan's own slug is the obvious value when the plan stays one group — the
     slug alone, with the `<YYYY-MM-DD>-` of the plan file's own name off it. A split group is
     named `<plan-slug>-<part>` instead, from step 2's own ballot. A group carrying a date pins
     the whole breakdown to the morning it was cut.
   - `source` — the issue's own URL, from `spoolway issue show`, when this breakdown started
     at one; the calling page's own absolute path otherwise, where one exists. The screen's
     `o` key opens a `source:` that is a URL; a path is carried for the reader rather than
     opened.
   - `plan` — the calling page's own absolute path, when there is both an issue *and* a page
     — an issue read straight into tasks with no page carries no `plan:` at all, and neither
     does a page with no issue behind it, since `source:` already carries its path there.
   - `base` — on every task, a dependent included: its chain's base from step 2, which is
     step 1's `base` unless a note changed it or the person picked the split ballot's
     recommended pull-request option for that chain. A dependency and its dependent must share
     one.
   - `depends_on` and `pipeline`.
   - `labels` — the plain words this task proposed on the ballot, carried through unchanged: the
     plan's own words, or the source issue's own labels when this breakdown started at one.
     Dropped for a task that proposed none.

   **Link a source; never redraw it.** Whatever the shape came from as a file — the calling
   page, a screenshot, anything the person named or pasted — is linked from the task, never
   copied or translated into Markdown. A lane reads nothing outside its worktree and the
   project home, so a file anywhere else is first copied into
   `<home>/plans/<group>/` and linked there.

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

4. **Verify the shape.** Walk each group as one line, in dependency order: exactly one task
   in it with no dependency inside the group, one with no dependent inside the group, and,
   for a group stacked on another, its first task naming that other group's own last task and
   nothing else across the two. **A join, or a group with two roots, has to be fixed before you
   go on**: re-run step 2 rather than patch ids after the fact.

   Whether two groups are safe to run side by side is yours to judge from what each one
   changes, not from a shared file — spoolway reports no overlap of its own, and `enter`
   writes both groups straight through with no `depends_on` invented. So say the pair out
   loud to the caller if you judged them safe beside each other on a shared file, and never
   tell them something downstream will check it; do not invent a section for it.

   **Then check each group's own last task carries a `last:` step.** A group's last task —
   the one nothing else in its own group depends on — is the only one whose run of the
   pipeline ever reaches a step marked `last-of-chain` in step 1's contract. When that task's
   own routed pipeline carries none, that group loses the step silently: nobody after it ever
   runs it, since every other task in the group walks straight past. Re-route that one task to
   the first pipeline from step 1's gathering that carries a `last-of-chain` step — there is no
   project default to prefer over the rest — and say the swap out loud to the caller instead of
   just writing a different `pipeline:` into its task.

5. **Prove the tasks.** `spoolway task contract --from <home>/pending` —
   the same validation `queue add --from` runs, stopping short of the save. Fix what it
   reports, re-run until it passes, never mention the loop to the human.

6. **Say one line**, once: `<n> tasks written to <home>/pending. Open the
   board to send them.` Never summarise the tasks themselves.

## Guardrails

- Never invent a goal, criterion or reference the shape it came from doesn't support, and
  never leave a task as the unfilled skeleton.
- Never write down a claim about the code, a command, a file or upstream data you have not
  checked — grep it, read it or run it first. A `## Context` line or criterion that turns out
  false, or one that contradicts the task's intent, goes back to the person, not into the task.
- Never write a task anywhere but the pending directory, and never write anything else
  there — it is a queue of tasks, not a scratch directory. The one other place this skill
  writes is `<home>/plans/<group>/`, and only a source file copied in
  to be linked.
- Never redraw a source in a task. A figure, a panel or a screenshot that exists as a file
  is linked; only what exists nowhere else is written out.
- Never call `spoolway queue add` or start a dispatcher here — writing a task is not
  queueing it; that is the screen's job, done separately by a human.
