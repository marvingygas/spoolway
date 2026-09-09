---
name: spoolway-doctor
description: Check whether a spoolway project would actually run — `spoolway doctor` and the other read-only checks, every finding collected into one report with a fix proposed for each, and nothing changed until the human picks. Triggered by a human who wants to know the pipeline is sound.
disable-model-invocation: true
---

# spoolway-doctor

Find out whether this pipeline would run, and say what to do about anything that would stop
it. Every command here reads; **nothing is changed until the human has approved it**.

**Diagnosing is not fixing.** A fix applied before the finding was read is a change nobody
agreed to, and several of the fixes here are not repairs at all — which branch work is based
on, whether unconfined lanes are acceptable, which model a profile runs. Those are the
project's decisions. Collect everything, report it once, and let the human pick.

**The checks are the easy half.** `doctor` prints only what is not `ok`, and the rest of
the value is in the reading: separating what stops a run from what a project meant, and
turning each into a fix concrete enough to approve.

## Procedure

1. **Run the checks — all of them, none that write.** From the project root, or any
   directory with `-C <dir>`. `doctor` and `queue list` take `--json`; parse that rather than
   their plain-text form. The other three do not — `spoolway pipeline check`, `spoolway
   prompt check` and `spoolway queue conflicts` still print for a person, so read them as
   text:

   ```
   spoolway doctor --json           # the preflight: config, pipelines, agents, documents, prompts
   spoolway pipeline check          # the graphs and the agents they name
   spoolway prompt check            # doctor only counts these findings; this prints them
   spoolway queue list --json       # the dispatcher, and where every task is sitting
   spoolway queue conflicts         # overlapping `touches` with no `depends_on` between them
   spoolway spend                   # what has been spent, and which models are unpriced
   ```

   Only `doctor` exits non-zero, and only for its own `fail` rows (`FAIL` in the plain-text
   form). The others report by printing. **Read the output; never conclude from an exit
   code.**

   Two more, when they apply:

   - `spoolway dispatch --dry-run` — what the next pass would actually do, spawning
     nothing and writing to no task file. Run it only when `queue list` says no dispatcher is
     running: it takes the same lock, and with one up it just says so.
   - `spoolway lane <lane> --json` — for a lane a task has been sitting on. Lanes are named
     `<task> · <step>`; `spoolway lane --json` with no argument lists them.

   If a command reports the project is not set up, stop there and say so — `spoolway init`
   is the answer, and it is the human's to run.

2. **Read each row for what it is.** `doctor --json` is `{checks, problems, rows: [...]}`,
   each row tagged `"kind"`, and only one kind is counted:

   - **`fail`** (`{kind, label, why}`) — this stops a run. Counted, and reported first.
   - **`note`** (`{kind, text, verbose_only}`) — never counted, and never automatically
     wrong. Some are a project's own settled choice; some are a real problem that could not
     be judged from inside the binary. Read every one. A report that says "3 problems" and
     drops nine notes has hidden the interesting half.
   - **`ok`** rows (`{kind, label, note}`) are left out unless `-v`/`--verbose` was also
     given — that flag is what turns a coverage question into an answer, not `--json` on its
     own.

3. **Collect before proposing.** One list. For each finding: the row (`doctor`, `queue
   list`) or the line (everything still printed), quoted as it actually came back, what it
   costs when left alone, and the fix. Then sort each fix into exactly one class,
   because the class is what the human is really approving:

   - **Mechanical** — restores what spoolway itself writes, with no judgement in it:
     `spoolway init` for a missing prompt or skeleton (it writes only what is absent), `spoolway update` for one that is present but behind (it replaces only the
     generated block and copies their prose through unread — `--dry-run` first), or
     `spoolway config set <key> <value>` where doctor named both the key and the value it
     will accept. **Prompts are never mechanical.** Nothing updates one, so a finding
     about prompt prose is always the project's text to change.
   - **A decision** — the fix is a choice about how this project works. Present the real
     options and what each costs; **never pick one**.
   - **Outside spoolway** — a missing agent CLI, no multiplexer, no remote. Say what is missing and what would provide it. Running it is theirs.

4. **Report, then ask.** Failures first, in the order they would stop a run; then the notes
   worth acting on, with what each one actually risks; then one line for the rest
   (`n checks ok`). Never list `ok` lines. One entry per finding, in this shape:

       FAIL  <the line, exactly as the command printed it>
             Costs: <what it costs while it is left alone>
             Fix:   <the command to run, or what a person would have to do>
             Class: mechanical | decision | outside spoolway

       note  <the line, exactly as the command printed it>
             Costs: <what it risks — or "settled choice", and nothing else>
             Fix:   <…>
             Class: <…>

   Then use **request_user_input**: the mechanical fixes batched as one option and named
   individually in its description, each decision as its own question with its real
   options, and in every case an option that changes nothing. If nothing failed and no note
   is worth acting on, say so in a line and stop — do not manufacture work to have a
   recommendation.

5. **Apply only what was approved, then re-check.** Run what was picked and nothing
   adjacent, however obvious it looks from here. Then `spoolway doctor` again and show the
   difference: what cleared, what is still open, what the fix surfaced. Anything declined
   stays in the report as declined.

## What the findings mean

| Reported | What it costs | Fix | Class |
|---|---|---|---|
| ``agent `x` has a model: no model set`` | lanes on that profile start with an empty model | name a `model:` on each step that runs on `x`, in `.spoolway/pipelines/<name>.yml` (`agents.*.model` is retired) | decision — the model is theirs |
| ``agent `x`: `pi` is not on PATH`` | every step on that profile refuses to start | install the CLI, or point the profile at one that is installed | outside |
| ``agent `x` permission mode: … is not a mode`` | the lane dies on an unrecognised flag, passes into a run | `spoolway config set agents.x.permission_mode <one it lists>` | mechanical |
| ``prompt for `step` is missing`` | that step cannot start a lane | `spoolway init` | mechanical |
| ``lanes can be started: …`` | there is nowhere to run a lane | install the multiplexer, or `dispatch.backend = "headless"` | decision |
| ``pipelines are valid`` fails | nothing dispatches | edit `.spoolway/pipelines/`, confirm with `spoolway pipeline check` | decision |
| ``task dependency graph`` fails | a cycle or an unknown `depends_on`; those tasks never start | edit the task's frontmatter in the project's own `queue/` | decision |
| note: no git remote | nothing can be handed over: every task's change goes out as a pull request | add the remote | outside |
| ``prompt contract``: ``profile `x` never uses `{prompt_file}` `` | the prompt file is written but never handed to the agent | point the profile at a `kind` spoolway knows (the argv per kind is fixed in the binary; `args` is retired) | decision — the kind is theirs |
| note: `dispatch.max_launches` / `max_attempts` retired | the launch guard it sized is a constant now (one launch, then a person) | nothing to set — the key is dropped on the next `config` save | mechanical |
| note: `n prompt finding(s)` | prose a lane will act on that its step does not permit, or a command this spoolway does not have | report what `spoolway prompt check` printed, not the count | decision |
| ``prompt names `spoolway <verb>` `` | that lane runs a command this release does not have, and finds out mid-run | fix the prose, or `spoolway update --replace .spoolway/prompts/<name>/PROMPT.md` to take the shipped one | decision — it is their text |
| `queue list --json`: `"state": "blocked"` | it is out of the pipeline until someone puts it back | address the blocker it names, then `spoolway resume <task>` | decision |
| `queue list --json`: `"state": "waiting_on_you"` | a lane asked a question and is holding its pane | answer it in the pane | theirs |
| `queue list --json`: `"laps"` non-null | a task looping between two steps | say how many rounds and on which step; the cause is in the lane's log | decision |
| `queue conflicts` | two lanes would edit the same files | add `depends_on` to the later task's frontmatter | decision |
| `spend`: unpriced models | spend that cannot be seen | price the model in config, or accept it knowingly | decision |

## Never

- Never run anything that writes before it was picked — not `init`, not `config set`, not
  `git checkout`, not `resume`, not `dispatch`.
- Never `spoolway init --force`. It overwrites an edited prompt, the pipeline and the
  config. Plain `init` writes only what is missing, which is the whole of the fix — and
  where the fix is "this project is behind", it is `spoolway update`, which never touches
  a prompt at all.
- Never start a dispatcher from here. `--dry-run` is a check; `spoolway dispatch` is
  the pipeline running, and that is a human's call.
- Never silence a finding by lowering the bar it failed — loosening a review standard,
  widening a `touches`, dropping a `depends_on`. That makes the failure disappear without
  changing anything it was reporting.
- Never guess a model name, a branch name, or a value doctor did not print.
- Never call it clean on doctor's exit code. Notes do not count toward it, and blocked
  tasks, `touches` conflicts and unpriced models are not doctor's checks at all.
