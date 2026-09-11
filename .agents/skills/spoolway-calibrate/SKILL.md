---
name: spoolway-calibrate
description: Read a window of archived tasks and the spend ledger inside it, work out what actually cost loops and money — a line that should go, a step running hot on context, a lap no step owns — walk the findings with the person one at a time, and send each where they choose: cut as a task, planned first, or reshaped as a pipeline. Triggered by a human who wants to know what the control plane — its pipelines, prompts, task skeletons and config — is costing in practice.
disable-model-invocation: true
---

# spoolway-calibrate

Find out what the control plane actually costs, from the record of running it — not from
reading the prompts and pipelines and guessing. Every command here reads. **This skill writes
nothing itself** — a finding the person sends on is written by the skill it goes to,
`spoolway-tasks`, `spoolway-plan` or `spoolway-pipeline`.

**A finding with no number behind it is not a finding.** "This prompt seems long" is an
opinion. "3 lines become 1, saving about 40 tokens on every one of the 25 lanes that read it
this window" is one. So is "`implement` peaked above 90% of its window on 9 of 14 runs, and 6
of those went back a step" — that number is a count off the record rather than a token delta.
Anything this procedure cannot put a number against is dropped before the person ever sees it.

Two kinds of finding are in scope, and the second matters as much as the first:

- **A cost finding** tightens a line: what changes, and what keeping the old line cost.
- **A shape finding** changes the flow: a step carrying more than one pass can hold and
  wanting a split, or a lap that keeps recurring because no step owns the work it redoes.
  Its fix is a pipeline change, and it goes to `spoolway-pipeline`.

## Procedure

1. **Read the window.** `spoolway config get calibrate.window` — the duration back from now
   this session reads. Then count, without yet reading their contents: archived task
   documents whose Status Log last moved inside the window, the ledger rows `spoolway eval`
   already groups for the same window, and the control plane's own files —
   `.spoolway/pipelines/*.yml`, `.spoolway/prompts/*/PROMPT.md`,
   `.spoolway/templates/tasks/*.md`, and `.spoolway/config.toml`. Report before anything else,
   in this shape:

       calibrate.window = 14d

       archive    <n> tasks finished in the window
       ledger     <n> lanes · $<total> on pipeline steps
       control    <n> pipelines · <n> prompts · <n> skeletons · 1 config

       reading <n> task documents, the ledger, and the files they point at …

2. **Read the archive.** Every task document under `~/.spoolway/<project>/archive/` whose
   Status Log last moved inside the window — `<project>` is the basename of the repo root.
   Each one's frontmatter carries `rounds` (laps of the loop, keyed `from->to`) and `prompts`
   (lanes launched, same keys); its body carries the Status Log a lane's own words about
   why it went back a step, and the Handoff notes lanes left each other. Read all of it —
   the evidence a finding cites has to be a real line from a real document, not a summary of
   one.

3. **Read the ledger.** `spoolway spend step --since <window>` prices exactly what step 1
   counted; call it rather than recomputing anything from the raw usage records. A step named
   in more than one pipeline is one row here — that is the ledger's own grouping, and this
   procedure does not second-guess it. Then `spoolway eval --runs --since <window>` for one row
   per run: its CTX PEAK column is how close a lane came to filling its model's window, and
   BLOCKS is how often one gave up.

4. **Look for what repeats.** A lap that keeps recurring on the same route is the strongest
   signal — sum `rounds` across the archived documents by `from->to` and rank by how much the
   ledger prices the route's own steps. A Handoff note repeated near-verbatim across several
   tasks is the same signal in prose: whatever it kept having to say, something upstream of
   it should already have said. A step whose CTX PEAK sits near its window across most of its
   runs is the third signal: one pass is holding more than it can, and the work wants splitting.

5. **Turn a pattern into a finding, or drop it.** For each candidate, go read the actual file
   its fix would touch and write the fix against real lines, not a description of them:

   - **A finding that removes or narrows something** names the exact lines being replaced or
     deleted, the net line count, and the token delta — estimate roughly four characters to a
     token — multiplied by how many lanes in the window's ledger read that file: a line struck
     from a prompt is paid back once per lane that runs it, and a line struck from a pipeline
     is arithmetic without a token count (`net 0`) rather than none at all.
   - **A finding that only adds** names the existing line that failed to cover the case it is
     fixing, and the tokens the addition costs per lane the same way.
   - **A shape finding** names the step, the runs it is read off, and the new shape: one step
     split in two, or a step the pipeline does not have yet where a lap keeps redoing work no
     step owns. Its arithmetic is the laps and blocked lanes it would end, priced by the
     ledger — a pipeline change carries no token delta of its own.

   A pattern that cannot produce one of these — because no line actually accounts for it,
   or because the fix would touch a file this procedure has no standing to touch — is dropped
   here, silently. It never reaches the walk in step 7.

6. **Sort every survivor by scope.** A finding whose fix sits under `.spoolway/pipelines/`,
   `.spoolway/prompts/`, `.spoolway/templates/tasks/` or is `.spoolway/config.toml` itself is
   in scope and may be kept. Anything else — a source file, a script, a document outside the
   control plane — goes on an out-of-scope list instead: reported so the pattern is not lost,
   and never cut as a task.

7. **Walk the findings with the person**, ranked by what they cost, each as one block:

       <n>  <the pattern, one line>
          evidence  <the rounds or Handoff lines this is read off, with counts>
          costs     <what the ledger prices it at, and what else it costs — wall time, a lane>
          fix       <file>:<lines>
                    <the line change>  net <±n> · ~<±tokens> tok × <n> lanes

       out of scope, reported only
          <pattern> <n> times — <why it is not this skill's fix>

   Walk them one at a time, never batched — a person approving a table they have not actually
   read is the same failure a mechanical fix from `spoolway-doctor` would be here, and these
   fixes edit prose and routing, not restore a generated block. Put each finding to them with
   **request_user_input**, the one you recommend first:

   - **Cut it as a task** — the lines and the arithmetic are both settled. Goes to step 8.
   - **Reshape the pipeline** — invoke **spoolway-pipeline** with this finding as its goal: it
     owns the graph and the prompts the steps run, and it decides the split or the new step.
     Recommend this for every shape finding.
   - **Plan it first** — invoke **spoolway-plan** with this finding as its goal, and let that
     session decide what follows. Recommend this when the fix crosses more than one file, or
     reopens a decision rather than tightening a line.
   - **Drop it** — the cost is real, and not worth paying down.

   Say which you recommend and why, in one line, before the question.

8. **Hand the cut findings to `spoolway-tasks`.** Each one becomes one task: the fix's file
   and lines are the Goal, the arithmetic is the acceptance criterion, and the archived
   evidence is the reference. This skill decomposes nothing and writes no document
   itself — invoke `spoolway-tasks` and let its own procedure settle the shape, the pipeline,
   and where the documents land.

9. **Say one line**, once: what went to `spoolway-tasks`, `spoolway-plan` and
   `spoolway-pipeline`, what was dropped, and what stayed on the out-of-scope list. Never
   repeat the findings themselves — the walk in step 7 already said them once.

## Never

- Never edit anything under `.spoolway/` from this skill. Writing is `spoolway-tasks`',
  `spoolway-plan`'s and `spoolway-pipeline`'s job — never `Write` or `Edit` called directly on
  a pipeline, a prompt, a task skeleton, or `config.toml`.
- Never invent a spoolway command to read or summarise the archive. Read the documents with
  the file tools directly; `spoolway eval` and `spoolway spend` are the only commands this
  procedure calls, because they are the only ones that price the ledger.
- Never carry a finding with no number behind it, and never let the person choose without
  seeing the fix's exact lines or the exact step it moves.
- Never cut a finding whose fix sits outside the declared scope — report it and move on.
- Never batch findings into one task. Each cut finding is independently completable, one fix,
  and `spoolway-tasks` decides the rest.
