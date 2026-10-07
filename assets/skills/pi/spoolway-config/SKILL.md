---
name: spoolway-config
description: Write, change or repair anything spoolway runs on — a pipeline, a step, the PROMPT.md a step's lane is briefed with, `config.toml`, the task and lane templates, a cron job, a routine, a shared skill, or an issue hook. Use it whenever somebody wants a new pipeline, a new or rewritten prompt, a scheduled job, a routine to queue, or a different model, effort, timeout or concurrency anywhere in spoolway — said as override, try, tweak, change or set. Use it too when a project is broken, refused or behind, or when somebody wants to set a repo up without touching it, join a workspace, start a project from nothing, or try a pipeline privately before it goes in the repo. Fetches every format from the binary itself and checks what it wrote.
---

# spoolway-config

## How spoolway works

- project        git checkout spoolway runs on
- setup          config.toml, pipelines/, prompts/, templates/, routines/, jobs.toml
  - repo mode    .spoolway/ in the checkout, tracked
  - home mode    a workspace's config/, nothing in the checkout
- workspace      ~/.spoolway/<label>-<id>/: one config/ shared by its clones
- local/         private copies, this machine only, repo mode only
- overrides/     patch layer over tracked files, never committed
- pipeline       pipelines/<name>.yml: ordered steps a task walks
- step           agent + model + prompt (a lane), or run: (a command)
- prompt         prompts/<role>/PROMPT.md: one role, any step, any pipeline
- skill          knowledge several prompts share; a step's skills: runs it
- task           one file, one goal, one pipeline
- task template  templates/tasks/<pipeline>.md: headings + bracketed placeholders
- routine        routines/<name>/: finished tasks, queued again with fresh ids
- job            cron schedule + pipeline + routine, fired by the dispatcher

## Before you write

    spoolway pipeline contract | list | show | check
    spoolway prompt contract --pipeline <name>    lane contract, prompt shape
    spoolway prompt list | show <name>            prompts, steps that run them
    spoolway task contract                        task frontmatter, routine layout
    spoolway template contract                    task, lane-prompt, PR shapes
    spoolway jobs contract                        job stores, keys, cron grammar
    spoolway jobs list                            every job, next firing
    spoolway config contract | list | get         settings and values
    spoolway config path --json                   every place the skill writes
    spoolway override contract | list             the patch layer
    spoolway agent list | verify <kind>           agent kinds this binary runs
    spoolway hook contract                        an issue hook's environment
    spoolway queue list                           what stands on which step
    spoolway doctor --json                        what is broken

- Never write a format from memory — every command above is printed.
- `config path --json` is how every route below finds its folders — never
  build one of these paths by hand.
- A task template is a skeleton, not a task: headings, a guidance paragraph
  per heading, `[[bracketed]]` placeholders. A finished task's own goal,
  criteria or findings never go in it.
- A pipeline whose tasks always come from a routine: no task template, no
  `task_template:` key.
- Name a new prompt by role, never by pipeline — `reviewer`, not
  `<pipeline>-reviewer`. Keep it to 40 lines; past that, trim or move the
  shared knowledge into a skill.
- A prompt never restates or contradicts what `prompt contract` shows every
  lane is told. Check each prompt you write or change against it, and cut
  the overlap.

## Preferences

- Planning, research and review: the strongest model available, high effort.
- Implementation and documentation: the cheapest model that can do the job —
  a local one where the contract says local models are in play.
- Fewer steps where a prompt is one role, one pass, one thing to produce.
  Offer splitting to the person where a prompt wants two of those.

## Procedure

- Read what the project has, from the commands above, never documentation.
- Four routes answer everything this skill is asked for: edit the setup for
  everyone, make a private copy just for this machine, tweak one value in
  place, or run `init` to set up, join a workspace, or start from nothing.
  Pick the one the person's own words point at, and say which one you took.

**Edit the setup, for everyone** — a new pipeline, a new step, a step split
off an existing one, a job, a routine, or a value made permanent:

- Ask what you cannot decide: print the question, then end the turn — pi
  has no dialog tool, so the answer comes back as the person's next prompt.
  2–3 concrete lettered options each.
- Write the pipeline from the contract's own template, a prompt for every
  step that does not already have one, and a task template skeleton — skip
  it, and the `task_template:` key, where every task on this pipeline
  always comes from a routine — into the setup folder `config path --json`
  prints.
- Every pipeline gets a `description:`, taken from the person's own words
  verbatim, compressed to one sentence when it runs longer.
- A routine: one folder under `config path`'s `routines`, one finished task
  per file, `depends_on` only a sibling in the same folder.
- A job: one `[jobs.<name>]` table, per `spoolway jobs contract`, in the
  store the person picks — `jobs.user` for just this person, `jobs.project`
  for everyone. Check it with `spoolway jobs list`.
- A layered patch already right and ready to keep: `spoolway override
  promote <target>`, not retyped.
- Check it clean: `spoolway pipeline check`.
- Say what changed: the paths touched, and `spoolway pipeline show` where a
  step moved.

**Try it, just for me** — a pipeline or prompt tried privately, with nothing
written to the tracked setup, repo mode only:

- The person's own request to try something is already the go: run
  `spoolway pipeline copy <from> <to>` straight away, with no separate
  question first. Ask first only when you are the one proposing a private
  copy the person never asked for.
- `pipeline copy` copies a pipeline and its task skeleton into `local/`;
  `spoolway prompt copy <from> <to>` copies a prompt the same way. Edit the
  private copies exactly as you would a tracked file.
- Check it clean: `spoolway pipeline check`.
- Say it is private, and name the exact path under `local/`.
- "Make it part of the repo" is already the go for `spoolway pipeline
  promote <name>` too — run it, do not ask again. It moves the pipeline and
  every private prompt it names into the tracked setup and deletes the
  private files. Nothing is committed by this skill.
- Home mode has no `local/`: `pipeline copy` writes straight into the
  workspace's `config/`, and `promote` is refused.

**Tweak one value here** — a model, an effort, a timeout, a concurrency, a
whole prompt, or any key `spoolway config contract` lists:

- Reach for the override layer by default:
  - a step's field: `spoolway pipeline override <name> --set <step>.<key>=<value>`
  - a whole prompt: `spoolway prompt override <name>`, then edit the fork
  - a config key: `spoolway config override`, or set one key by hand in
    `overrides/config.toml`
- Say which layer you wrote, every time, and offer to undo it: `spoolway
  override drop <target>`.

**Set up, join a workspace, or start from nothing** — `spoolway init`:

- The person's own request — "set this repo up", "join the workspace" — is
  already the go to run `init`; what still needs asking is whatever it did
  not answer.
- Ask "Where should this project's setup live?" if unsaid, then always pass
  `--setup repo` or `--setup home` and `--yes`.
- In home mode, ask which workspace: an existing one `config path --json`'s
  `workspaces` lists, or `--workspace new`.
- Ask "Install the example setup?" and pass `--examples` or `--no-examples`.
- Without examples, write the first pipeline, its prompts and a task
  template skeleton yourself, from `pipeline contract`, `prompt contract`
  and `template contract` — the same shapes "Edit the setup, for everyone"
  writes from — into the setup folder `config path --json` now prints.
- Check it: `spoolway doctor --json`.
- Say what happened: which workspace was joined, or that the project is
  set up, and its `config/` path.

**Repairing a project** — something is broken, refused or behind:

- Read `spoolway doctor --json` and `spoolway pipeline check`.
- Propose every fix in one list, each marked as keeping behaviour or
  changing it. Apply only what the person confirms.
- Run both again and show what cleared.

**Shared knowledge, moved into a skill** — facts two or more prompts repeat:

- Propose it: print the question, then end the turn — the exact lines it
  would move, the path it would write, and which steps' `skills:` would
  gain it.
- On yes: write the skill into the provider's skill folder — the checkout's
  in repo mode, the user-level one in home mode — with
  `disable-model-invocation: true` in its frontmatter on claude and pi,
  none on codex.
- Add the skill's name to every step's `skills:` named in the proposal.
- Check it clean: `spoolway pipeline check`.
- Name the path written, to the person.

## Never

- Never rewrite prose the human brought — transcribe it, and flag what
  would stop the pipeline running instead of fixing it yourself. Exception:
  a `description:` longer than one sentence is compressed to one.
- Never edit a shipped prompt to make a new role — a role is a file of its
  own, written from the contract's shape.
- Never queue tasks or start a dispatcher.
- Never rename or delete a step a task is standing on without saying so —
  `spoolway queue list` is how you find out.
- Never reach past what `config path --json` prints, or — once the person
  says yes — the provider's skill folder.
- Never let `init` run with no terminal to answer its questions. Always ask
  the person which setup place they want first, and always pass `--setup`
  and `--yes`.
- Never move a private file into the tracked setup, or run `init`, without
  the person's own go — but an explicit request already is that go: "make
  it part of the repo" for `pipeline promote`, "try this privately" or "set
  this up" for `pipeline copy` or `init`. Ask first only when you are the
  one proposing the move, the copy or the setup.
- Never write a skill file without the person's yes to that exact proposal.
- Never teach a model choice to a lane's own prompt.
- Never put a spoolway coupling in a pipeline, a prompt or a command: a
  `run:` line, a script it calls or a prompt names no `SPOOLWAY_TASK`,
  `SPOOLWAY_TASK_FILE`, `SPOOLWAY_REPO`, `SPOOLWAY_STEP`, `SPOOLWAY_WORKTREE`
  or `SPOOLWAY_SCRATCH`, and no `spoolway report`. Any other `SPOOLWAY_`
  name or subcommand is legal. `spoolway stack` on a handover step is the one
  `run:` exception, and an issue hook is outside the rule.
- Never hand the person a spoolway command. Run it yourself once they say
  yes.
