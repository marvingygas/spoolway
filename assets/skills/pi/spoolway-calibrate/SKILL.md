---
name: spoolway-calibrate
description: Read a window of archived tasks and the spend ledger inside it, work out what actually cost loops and money — a line that should go, a step running hot on context, a lap no step owns — walk the findings with the person one at a time, and send each where they choose: cut as a task, planned first, or reshaped as a pipeline. Triggered by a human who wants to know what the control plane — its pipelines, prompts, task skeletons and config — is costing in practice.
disable-model-invocation: true
---

# spoolway-calibrate

Find out what the control plane actually costs, from the record of running it — not from
reading the prompts and pipelines and guessing. Every command here reads. **This skill writes
nothing itself** — a finding the person sends on is written by the skill it goes to,
`spoolway-tasks`, `spoolway-plan` or `spoolway-config`.

**A finding with no number behind it is not a finding.** "This prompt seems long" is an
opinion. "3 lines become 1, saving about 40 tokens on every one of the 25 lanes that read it
this window" is one. So is "`implement` peaked at or above 60% — very high — on 9 of 14 runs,
peak 74%, and 6 of those went back a step" — that number is a count off the record rather than
a token delta. Anything this procedure cannot put a number against is dropped before the person
ever sees it.

Two kinds of finding are in scope, and the second matters as much as the first:

- **A cost finding** tightens a line: what changes, and what keeping the old line cost.
- **A shape finding** changes the flow: a step carrying more than one pass can hold and
  wanting a split, or a lap that keeps recurring because no step owns the work it redoes.
  Its fix is a pipeline change, and it goes to `spoolway-config`.

## Procedure

1. **Read the window.** `spoolway config get housekeeping.calibrate_window` — the duration back from now
   this session reads. Then count, without yet reading their contents: archived task
   documents whose Status Log last moved inside the window, the ledger rows `spoolway eval`
   already groups for the same window, and the control plane's own files —
   `.spoolway/pipelines/*.yml`, `.spoolway/prompts/*/PROMPT.md`,
   `.spoolway/templates/tasks/*.md`, and `.spoolway/config.toml`. Report before anything else,
   in this shape:

       housekeeping.calibrate_window = 14d

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

   **A run at or above 60% CTX PEAK is very high.** The reading happens at a turn boundary, and
   the next turn can overshoot it materially, so 60% is a firm line even for a single run — never
   softened to "near its window". For every step, count how many of its runs in the window
   cleared 60% against how many ran at all, and carry the single highest peak among them. Report
   every step that clears the line at least once, in this shape, before step 4:

       implement  14 runs · 9 at or above 60% · peak 74%
                  very high — inspect model fit or split the step

   One run over the line is evidence to look at on its own; it does not by itself prove the step
   needs splitting. Only a step that clears 60% repeatedly, or whose crossings line up with the
   laps or blocks counted in step 4, earns a shape finding.

4. **Look for what repeats.** A lap that keeps recurring on the same route is the strongest
   signal — sum `rounds` across the archived documents by `from->to` and rank by how much the
   ledger prices the route's own steps. A Status Log or Handoff line repeated near-verbatim
   across several tasks is the same signal in prose: whatever it kept having to say, something
   upstream of it should already have said. The CTX PEAK classification from step 3 is the third
   signal: a step clearing 60% repeatedly, or lining up with the laps and blocks just summed, is
   one pass holding more than it can, and the work wants splitting.

   For every repeating statement kept as a candidate, quote it or name the task it came from and
   the exact line it paraphrases, together with how many tasks say it — `"three trips through
   e2e" (task-471, task-483, task-502)` is a citation; `"e2e often reruns"` is not. A statement
   that appears once, with no repetition and no lap or block count behind it, is not a candidate.

5. **Run every candidate through the full sweep before deciding.** A repeated statement can look
   like one thing and be caused by another. Before a candidate is kept or dropped, check it
   against all six of these, and record in the finding which lever held and that the rest were
   checked and rejected:

   - **step ownership/routing** — does the step re-running actually own the failure, or does it
     belong upstream or downstream of where the pipeline currently sends it?
   - **prompt gaps** — does the step's own prompt already cover the case, or is the repeat proof
     that it doesn't?
   - **task size and criteria** — is the task arriving too large, or under-specified, for one
     pass of this step to close?
   - **loop/gate placement** — is the loop or gate that keeps firing wired to the right step, or
     catching work that step cannot itself fix?
   - **model/context fit** — is the step's CTX PEAK reading (step 3), or the model assigned to
     it, the actual reason it keeps failing or running long?
   - **repeated recovery work** — is a block or a retry the record of one honest failure, or the
     same recovery paid for again on the same route?

   A candidate where every lever comes back negative is dropped here, silently — it never
   reaches the walk in step 8. One where a lever holds becomes a finding, with that lever named
   as its diagnosis.

6. **Turn a surviving candidate into a finding.** Go read the actual file its fix would touch
   and write the fix against real lines, not a description of them. Every finding carries all
   of the following:

   - **lane record** — the statement(s) quoted or precisely identified in step 4, with the task
     and occurrence count behind them.
   - **evidence** — the rounds or runs this is read off, with counts.
   - **diagnosis** — the lever from step 5 that held, and that the others were checked and
     rejected.
   - **technical** — the concrete engineering consequence the diagnosis explains — a wrong
     owner, an uncovered case, an oversized pass — not a restatement of the symptom.
   - **delivery** — the concrete effect on handover, operator recovery or throughput, counted off
     the same evidence. Never a customer or revenue claim; if the archive and ledger cannot
     number it directly, it stays out.
   - **costs** — what the ledger prices it at, and what else it costs — wall time, blocked
     recoveries, a lane.
   - **fix** — the exact file and lines, shaped by what kind of finding it is:

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

   A candidate that cannot produce all seven parts — because no line actually accounts for it, or
   because the fix would touch a file this procedure has no standing to touch — is dropped here,
   silently. It never reaches the walk in step 8.

7. **Sort every survivor by scope.** A finding whose fix sits under `.spoolway/pipelines/`,
   `.spoolway/prompts/`, `.spoolway/templates/tasks/` or is `.spoolway/config.toml` itself is
   in scope and may be kept. Anything else — a source file, a script, a document outside the
   control plane — goes on an out-of-scope list instead: reported so the pattern is not lost,
   and never cut as a task.

8. **Walk the findings with the person**, ranked by what they cost, each as one block:

       <n>  <the pattern, one line>
          lane record  <the quoted or identified statement, with task and occurrence count>
          evidence     <the rounds or runs this is read off, with counts>
          costs        <what the ledger prices it at, and what else it costs — wall time, a lane>
          diagnosis    <the lever from step 5 that held; the others checked and rejected>
          technical    <the engineering consequence>
          delivery     <the handover/operator-facing consequence, numbered off the same evidence>
          fix          <file>:<lines>
                       <the line change>  net <±n> · ~<±tokens> tok × <n> lanes

       out of scope, reported only
          <pattern> <n> times — <why it is not this skill's fix>

   Walk them one at a time, never batched — a person approving a table they have not actually
   read is the same failure a mechanical fix from `spoolway-doctor` would be here, and these
   fixes edit prose and routing, not restore a generated block. **Print each finding as its
   own question and end the turn on it** — pi has no dialog tool, so the answer comes back as
   the person's next prompt. The one you recommend first:

   - **Cut it as a task** — the lines and the arithmetic are both settled. Goes to step 9.
   - **Reshape the pipeline** — invoke **spoolway-config** with this finding as its goal: it
     owns the graph and the prompts the steps run, and it decides the split or the new step.
     Recommend this for every shape finding.
   - **Plan it first** — invoke **spoolway-plan** with this finding as its goal, and let that
     session decide what follows. Recommend this when the fix crosses more than one file, or
     reopens a decision rather than tightening a line.
   - **Drop it** — the cost is real, and not worth paying down.

   Say which you recommend and why, in one line, before the question.

9. **Hand the cut findings to `spoolway-tasks`.** Each one becomes one task: the fix's file
   and lines are the Goal, the arithmetic is the acceptance criterion, and the archived
   evidence is the reference. This skill decomposes nothing and writes no document
   itself — invoke `spoolway-tasks` and let its own procedure settle the shape, the pipeline,
   and where the documents land.

10. **Say one line**, once: what went to `spoolway-tasks`, `spoolway-plan` and
    `spoolway-config`, what was dropped, and what stayed on the out-of-scope list. Never
    repeat the findings themselves — the walk in step 8 already said them once.

## Never

- Never edit anything under `.spoolway/` from this skill. Writing is `spoolway-tasks`',
  `spoolway-plan`'s and `spoolway-config`'s job — never `Write` or `Edit` called directly on
  a pipeline, a prompt, a task skeleton, or `config.toml`.
- Never invent a spoolway command to read or summarise the archive. Read the documents with
  the file tools directly; `spoolway eval` and `spoolway spend` are the only commands this
  procedure calls, because they are the only ones that price the ledger.
- Never carry a finding with no number behind it, and never let the person choose without
  seeing the fix's exact lines or the exact step it moves.
- Never carry a finding that skips the lane record, evidence, diagnosis, technical or delivery
  part, and never call a step's context pressure a shape finding on one run alone — only
  repetition or a correlated lap or block count earns that.
- Never cut a finding whose fix sits outside the declared scope — report it and move on.
- Never batch findings into one task. Each cut finding is independently completable, one fix,
  and `spoolway-tasks` decides the rest.
