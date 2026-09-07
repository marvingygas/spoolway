---
name: spoolway-plan
description: Planning for spoolway. Work out what the human wants, settle every question, and write the shape as one self-contained plan page — offered, not assumed. Cuts the task breakdown into the pending directory once approved — nothing is decomposed while the shape is still argued.
disable-model-invocation: true
---

# spoolway-plan

Turn one goal into a decision record a human can approve, published as one self-contained plan
file. **The page is offered, never assumed** — a person who has settled the shape out loud may
go straight to cutting tasks. The breakdown into tasks happens only once approved, and lands
beside the page as task documents rather than on it — see "Cut the tasks" below.

**This is planning, until the human says otherwise.** Nothing here writes code or starts a
dispatcher — writing a task document does not queue it. Fixing something is a task for a lane,
not this session.

**`cp` and Edit reach exactly two places: the plan file, and the task documents step 7 writes.**
Not the source you just read to understand it, not the typo you noticed, not this skill. Read
anything; change nothing else — a fix made here is an unreviewed change on whatever branch the
session sits on, landing under a task describing a starting state the code is no longer in. What
you found while reading becomes a decision, a line of context, or a sentence to the human —
never an edit, and no request lifts this.

## Understand the intent, then settle every question

A person describing a detailed solution is telling you about a problem — take the solution as
evidence, not the specification. Work out what they are trying to **achieve**, and read enough
code to know how the system does that today; plan against the system as it is, not as
described. If their route isn't the best one to that outcome, say so in two sentences and offer
the alternative, then plan whichever they pick.

**The person is in the room now, and will not be later.** Every question is asked through
**request_user_input**, never written onto the page. **Do not write the page until every question
is settled out loud.**

- **Ask only what the code cannot answer** — read what the codebase settles; where one option is
  plainly better, take it and say so in a line. Ask what is left, if it would change the shape.
- **2–3 concrete options per question**, each a real route with its own consequence — "What do
  you think?" is a delay, not a question. **Ask in batches**, up to four per call.
- **A choice between two shapes gets drawn**, in the option previews.
- **The page is the output; the session is not.** Once it exists, chat output is one line plus
  step 6's question — a chat copy goes stale the moment the page is revised.

## A plan is a decision record

Every plan is written on the same spine, in this order:

| Section | What goes in it |
|---|---|
| **Context** | What is true today, and the pressure on it. One drawing of the system as it is. |
| **Decisions** | One record per decision, each led by a figure and closed by what it costs. |
| **Mockup** | The finished thing, drawn: the walkthrough, or the run end to end. One heading and figure per step, no prose. |
That is [Nygard's architecture decision record](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).
**The page ends at the Mockup.** The breakdown is not on it: a task is a document, written into
the pending directory at step 7, and the page argues the shape those documents were cut from.
Same headings, order and words on every plan.

**A record names a choice, not a component.** "Rename the module" is not a decision.
"Ownership moves to the run, not the task" is. The markup mechanics of a record live in
`assets/page.md`, not here.

## Draw first, then write the caption

**The reader is an architect, not a compiler**, approving a shape rather than reviewing a
patch, with no source in their head — argue in drawings and the artifacts a person opens, never
in prose about functions and line edits.

- **Every record has a figure, explained only after it**: forces, figure, what it doesn't show,
  the cost — nothing after re-explains it in words. Show code freely as supporting material
  under it, never first, never alone.
- **Four sentences per record before the cost line**, and the cost line is never skipped — a
  record with nothing to say it costs was not a decision.
- **The mockup section carries headings and figures, nothing else** — a two-or-three-word `h3`
  per step over the figure that shows it; a figure needing a sentence to land is not drawn yet.
- **A mockup starts from the thing's own captured output** — a screenshot, a real command's
  real output — never redrawn from memory. A panel may quote a command that runs today, never
  one that doesn't. **Writing or running anything to produce a panel's contents is refused** —
  a scratch script, a prototype, a measurement is implementation, and this session does not do
  that. Something that does not run yet has no output to capture, so its bar names the bound it
  was drawn to hold — and the task that builds it carries proving that bound as a criterion.

Catch the paragraph that defends a decision instead of stating it ("this matters because…") in
your own draft, and draw the mechanism rather than narrating it. Verbosity's home is the task
document written later, not this page: terse here, complete in the document.

**Reference, never restate.** A paragraph explaining how the system works today is stale the
day it changes; a path stays true. Name the file that already holds the ground needed, in the
`refs` list on context and on each record.

## Procedure

1. **Understand the goal**, per above. When the goal names an issue — a ref, a link, "issue
   57" — run `spoolway issue show <ref>` first and read its title, body, labels and comments
   before anything else: a correction or a scope cut often lives in a comment, and planning
   off the body alone plans off a stale draft. Then read the relevant code yourself before
   proposing anything — where that survey is genuinely large, offer **one or two read-only
   research subagents** with request_user_input, and spawn them only on a yes.

2. **Settle everything, before a single line of the page is written.** List every open question
   and put them to the person with **request_user_input**, batched.

3. **Ask whether the page is wanted.** Once the shape is settled, put it to them with
   **request_user_input** — *Write the plan page?* — two answers: **Write it** (step 4), or **Cut
   the tasks now** (step 7, no page written). A shape settled out loud is enough to cut from.
   The page is for a decision somebody has to approve, or come back to months later.

4. **New plan: copy the page, then fill it.** The skeleton at `assets/template.html`, beside
   this skill, decides what a plan looks like. **`assets/page.md`, beside it, is the mechanics
   reference: the default path, what every `[[slot]]` takes, how a record's and mockup's markup
   is built, and the proof block to run before you say a word. Open it now.**

   Fill every slot from a line-numbered grep for its marker, plus your own edits — never from a
   whole read of the page:

   ```
   grep -n '\[\[' <path>
   ```

   Fill only Context, Decisions and Mockup. The page never carries tasks at all — not before
   approval, not after.

5. **Revising an existing plan is a proposal, not an edit.** Read the file first, then, before
   touching it: **summarise the change**, section by section, two or three lines each, not a
   diff; **ask whatever it raises**, through request_user_input; and **wait for the person to
   confirm** — "Apply it" or equivalent authorises the edit. `assets/page.md`'s "Revising a
   plan" has the rest: Edits only, the proof block re-run, and what to say when a breakdown has
   already been cut from the shape being revised. A new plan's first copy skips all of this.

6. **Open it, say one line, and ask.** `wslview`, `xdg-open` or `open` — first that exists —
   then the path and nothing else: `Plan is ready: /abs/path/to/page`.

   **Then put the approval to them, with request_user_input, in the same turn** — never skipped,
   never replaced by an inviting sentence: a session that publishes and stops has handed
   somebody a plan with no tasks and no sign a step is outstanding. One question — *Cut the
   tasks for this plan?* — three answers: **Cut them now** (step 7, this session), **Revise it
   first** (step 5, question returns once landed), or **Leave it for now** (nothing cut).

   **Only the first time for the line** — a revision repeats the line and the question.

7. **Cut the tasks — only once approved**, step 6's question coming back *cut them now*, or
   step 3's answer skipping the page — never assumed from the page simply existing.

   Invoke the **spoolway-tasks** skill to do it: the shape it cuts into documents is the
   page's Decisions and Mockup, or — where no page was written — what step 2 settled. Where
   step 1 read an issue, its own URL is the `source:` each document gets and the page's own
   absolute path moves to `plan:` instead; with no issue behind it, the page's path stays
   `source:`, exactly as before `plan:` existed, and there is no `plan:`. Neither key is set
   when neither exists. `spoolway-tasks` decomposes, offers the shape, settles the pipeline,
   reads the skeleton, writes the documents, verifies the chain and proves the set with
   `spoolway task contract` — the whole of the procedure and its guardrails live there now, so
   that a second caller cutting a breakdown shares it rather than drifting from it.
