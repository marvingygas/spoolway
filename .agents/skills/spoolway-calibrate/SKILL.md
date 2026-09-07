---
name: spoolway-calibrate
description: Read a window of archived tasks and the spend ledger inside it, work out what actually cost loops and money, walk the findings with the person one at a time, and send each where they choose — cut as a task, or planned first. Triggered by a human who wants to know what the control plane — its pipelines, prompts, task skeletons and config — is costing in practice.
disable-model-invocation: true
---

# spoolway-calibrate

Find out what the control plane actually costs, from the record of running it — not from
reading the prompts and pipelines and guessing. Every command here reads. **This skill writes
nothing itself** — a finding the person sends on is written by the skill it goes to,
`spoolway-tasks` or `spoolway-plan`.

**A finding without arithmetic is not a finding.** "This prompt seems long" is an opinion.
"3 lines become 1, saving about 40 tokens on every one of the 25 lanes that read it this
window" is a finding — it says what changes, and what keeping the old lines cost. Anything
this procedure cannot reduce to that shape is dropped before the person ever sees it.

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

3. **Read the ledger.** `spoolway eval --by step --since <window>` and
   `spoolway eval --by skill --since <window>` price exactly what step 1 counted; call them
   rather than recomputing anything from the raw usage records. A step or a skill named in
   more than one pipeline is one row here — that is the ledger's own grouping, and this
   procedure does not second-guess it.

4. **Look for what repeats.** A lap that keeps recurring on the same route is the strongest
   signal — sum `rounds` across the archived documents by `from->to` and rank by how much the
   ledger prices the route's own steps. A Handoff note repeated near-verbatim across several
   tasks is the same signal in prose: whatever it kept having to say, something upstream of
   it should already have said.

5. **Turn a pattern into a finding, or drop it.** For each candidate, go read the actual file
   its fix would touch and write the fix against real lines, not a description of them:

   - **A finding that removes or narrows something** names the exact lines being replaced or
     deleted, the net line count, and the token delta — estimate roughly four characters to a
     token — multiplied by how many lanes in the window's ledger read that file: a line struck
     from a prompt is paid back once per lane that runs it, and a line struck from a pipeline
     is arithmetic without a token count (`net 0`) rather than none at all.
   - **A finding that only adds** names the existing line that failed to cover the case it is
     fixing, and the tokens the addition costs per lane the same way.

   A pattern that cannot produce one of these two — because no line actually accounts for it,
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

9. **Say one line**, once: what went to `spoolway-tasks`, what went to `spoolway-plan`, what
   was dropped, and what stayed on the out-of-scope list. Never repeat the findings
   themselves — the walk in step 7 already said them once.

## Never

- Never edit anything under `.spoolway/` from this skill. Writing is `spoolway-tasks`' and
  `spoolway-plan`'s job — never `Write` or `Edit` called directly on a pipeline, a prompt, a
  task skeleton, or `config.toml`.
- Never invent a spoolway command to read or summarise the archive. Read the documents with
  the file tools directly; `spoolway eval` is the only command this procedure calls, because
  it is the only one that prices the ledger.
- Never carry a finding with no arithmetic, and never let the person choose without seeing the
  fix's exact lines.
- Never cut a finding whose fix sits outside the declared scope — report it and move on.
- Never batch findings into one task. Each cut finding is independently completable, one fix,
  and `spoolway-tasks` decides the rest.
