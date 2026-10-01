---
name: spoolway-config
description: Write, change or repair anything spoolway runs on — a pipeline, a step, the PROMPT.md a step's lane is briefed with, `config.toml`, the task and lane templates, or an issue hook. Use it whenever somebody wants a new pipeline, a new or rewritten prompt, or a different model, effort, timeout or concurrency anywhere in spoolway — said as override, try, tweak, change or set. Use it too when a project is broken, refused or behind, or when somebody wants to set a repo up without touching it, join a workspace, start a project from nothing, or try a pipeline privately before it goes in the repo. Fetches every format from the binary itself and checks what it wrote.
---

# spoolway-config

Two different jobs share this skill because they share a graph: a step is
placement, a prompt is behaviour, and a value on either — a model, an
effort, a timeout, a concurrency — is what makes the step run the way
somebody wants. Designing a flow from scratch and tweaking one already
running are the same territory at different sizes.

The flow itself is the person's design. This skill sets one up from
scratch, or edits one they already run, and keeps every contract holding
while it does — it does not decide the shape for them, and it does not
decide which layer a change belongs in without saying so.

## Before you write

    spoolway pipeline contract    the graph: every key, every rule, a blank to copy
    spoolway pipeline show        every graph this project runs, defaults resolved
    spoolway prompt contract      what a lane is handed, and the shape to write
    spoolway config contract      every setting, its values, its default
    spoolway config path --json   where the setup, local/ and overrides folders live
    spoolway override contract    what a patch may say, and what it may not
    spoolway override list        what is layered right now, and over what
    spoolway template contract    the task, lane-prompt and PR shapes
    spoolway hook contract        the environment an issue-tracking hook is handed
    spoolway queue list           what stands on which step — renaming one strands it

Never write a format from memory. Every one above is printed, and the
printed one is what the loader enforces. `config path --json` is also how
every route below finds its three folders — never build one of these paths
by hand, since a project laid out differently from a guess would silently
route to the wrong place.

## Preferences

- Planning, research and review run on the strongest model available, at
  high effort. These steps decide what every later step does.
- Implementation and documentation run on the cheapest model that can do
  them — a local one where the contract says local models are in play.
- Prefer fewer steps, but not because fewer is always cheaper. A step
  asked to hold too much in one pass pays for it in laps, context
  pressure and blocked lanes, and splitting it can cost less than keeping
  it whole. Judge by the prompt: one role, one pass, one thing to
  produce. Where a prompt wants two of those, put splitting to the person
  as an option; otherwise leave the step whole.

## Procedure

- Read what the project has, from the commands above, never documentation.
- Four routes answer everything this skill is asked for: edit the setup for
  everyone, make a private copy just for this machine, tweak one value in
  place, or run `init` to set up, join a workspace, or start from nothing.
  Pick the one route the person's own words point at, and say which one you
  took.

**Edit the setup, for everyone** — a new pipeline, a new step, a step split
off an existing one, or a value made permanent:

- Ask what you cannot decide: print the question, then end the turn — pi
  has no dialog tool, so the answer comes back as the person's next prompt.
  2–3 concrete lettered options each. Always worth asking what one pass has to
  produce.
- Write the pipeline from the contract's own template, and a prompt for
  every step the project does not already have one for, into the setup
  folder `spoolway config path --json` prints.
- Every pipeline gets a `description:` — the sentence a reader chooses
  between pipelines by. Take it from the human's own words, verbatim except
  for the one narrow rewrite this skill ever makes to a human's prose:
  compressing it to a single sentence when it runs longer, per the exception
  below.
- A layered patch already right and ready to keep is promoted in, not
  retyped: `spoolway override promote <target>` writes it into the tracked
  file and clears the layer, an ordinary diff ready to review and commit.
- Check it clean: `spoolway pipeline check`, which reads every prompt
  against the steps that run it too.
- Say what changed: the paths touched, and `spoolway pipeline show` where a
  step moved.

**Try it, just for me** — a pipeline or prompt tried privately, with nothing
written to the tracked setup, repo mode only:

- The person's own request to try something is already the go: run
  `spoolway pipeline copy <from> <to>` straight away, with no separate
  question first. Ask first only when you are the one
  proposing a private copy the person never asked for.
- `pipeline copy` copies a pipeline and its task skeleton into `local/`;
  `spoolway prompt copy <from> <to>` copies a prompt the same way. Edit the
  private copies exactly as you would a tracked file.
- Check it clean: `spoolway pipeline check`.
- Say it is private, and name the exact path under `local/` — the person's
  machine only, not the repo.
- "Make it part of the repo" (or the same in other words) is already the go
  for `spoolway pipeline promote <name>` too — run it, do not ask again. It
  moves the pipeline and every private prompt it names into the tracked
  setup and deletes the private files. Nothing is committed by this skill.
- Home mode has no `local/`: there, `pipeline copy` writes straight into the
  workspace's `config/`, and `promote` is refused, because the whole setup
  is already private.

**Tweak one value here** — a model, an effort, a timeout, a concurrency, a
whole prompt, or any key `spoolway config contract` lists:

- Reach for the override layer by default. It is what leaves the checkout
  clean:
  - a step's field: `spoolway pipeline override <name> --set <step>.<key>=<value>`
  - a whole prompt: `spoolway prompt override <name>`, then edit the fork it writes
  - a config key: `spoolway config override` (opens `overrides/config.toml`),
    or set one key by hand in that file
- Say which layer you wrote, every time, and offer to undo it:
  `spoolway override drop <target>` for a layer.

**Set up, join a workspace, or start from nothing** — `spoolway init`:

- The person's own request — "set this repo up", "join the workspace" — is
  already the go to run `init`; what still needs asking is whatever it did
  not answer.
- Ask "Where should this project's setup live?" if the person has not said,
  then always pass `--setup repo` or `--setup home` and `--yes` — never leave
  either for `init` to ask at a terminal an agent cannot answer.
- In home mode, ask which workspace: the name of an existing one under
  `~/.spoolway/` to join, or a new one — then pass `--workspace <name>` or
  `--workspace new`.
- Ask "Install the example setup?" and pass `--examples` or `--no-examples`.
- Without examples, the project has no pipeline yet: write the first one,
  its prompts and its task template yourself, from `spoolway pipeline
  contract`, `spoolway prompt contract` and `spoolway template contract` —
  the same shapes "Edit the setup, for everyone" writes from — into the
  setup folder `spoolway config path --json` now prints.

**Repairing a project** — something is broken, refused or behind:

- Read `spoolway doctor --json` and `spoolway pipeline check`.
- Propose every fix in one list, each marked as keeping behaviour or
  changing it. Apply only what the person confirms.
- Run both again and show what cleared.

## Never

- Never rewrite prose the human brought. Transcribe it, and flag what would
  stop the pipeline running instead of fixing it yourself — with one
  exception: a `description:` longer than one sentence is compressed to
  one, since that field is what `spoolway pipeline show` renders and a
  reader chooses a pipeline by.
- Never edit a shipped prompt to make a new role — a role is a file of its
  own, written from the contract's shape.
- Never queue tasks or start a dispatcher. That is the queue screen's job,
  and a human's decision.
- Never rename or delete a step a task is standing on without saying so —
  `spoolway queue list` is how you find out.
- Never reach past the setup folder, `local/` and `overrides/` — the three
  paths `spoolway config path --json` prints. Every format this skill routes
  to is printed from inside them; a setting outside them is not spoolway's
  to change.
- Never let `init` run with no terminal to answer its questions. Always ask
  the person which setup place they want first, and always pass `--setup`
  and `--yes`.
- Never move a private file into the tracked setup, or run `init`, without
  the person's own go — but an explicit request already is that go: "make it
  part of the repo" is enough to run `pipeline promote` right away, and
  "try this privately" or "set this up" is enough to run `pipeline copy` or
  `init`. Ask first only when you are the one
  proposing the move, the copy, or the setup, rather than carrying out a
  request already given.
- Never teach a model choice to a lane's own prompt — the models and
  subagents a step's prompt spins up inside its own turn are outside
  spoolway entirely, appear in no ledger, and are not this skill's to
  configure. `agent:`/`model:`/`effort:` on a *step* are the whole of what
  spoolway sets.
- Never hand the person a spoolway command. Run it yourself once they say yes.
