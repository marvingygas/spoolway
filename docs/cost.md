---
domain: cost
covers: ["src/usage.rs", "src/spend.rs", "src/models.rs", "assets/model-prices.json"]
---

# Cost accounting

What the pipeline has spent, collected rather than estimated.

## How the numbers get there

spoolway never sees a token. It starts an agent in a pane and reads nothing that agent says
to a model. It does not have to: the agents already write a transcript with per-turn token
counts, so the only real problem is knowing **which transcript belongs to which task and
step**.

The answer is always a name spoolway chose, never a guess from a working directory and a
timestamp — but there are two ways to arrange that, because not every CLI will take an id.
`pi` and `claude` accept one, so the dispatcher mints one per lane and passes it through as
`{session_id}`, then looks for the transcript whose filename carries it. `codex` mints its
own and refuses any other, so spoolway pins it the other way: `$CODEX_HOME` gives that lane's
session a *directory* named after the id spoolway minted, and the transcript inside it is
unambiguous whatever codex called it. See
[Two ways to pin a session](agents.md#two-ways-to-pin-a-session).

When a lane settles, a line is appended to `usage.jsonl`, under the project's own home
(`~/.spoolway/<project>/`), and never rewritten — so the record outlives the task file, which
cleanup archives. If more turns land in that transcript afterwards, a later read of the
ledger appends one more line for them — see [Settled lanes](#settled-lanes).

Where that lookup is declared is the kind's own row — one row per kind, in `agent::ADAPTERS`,
spanning how it launches and how it is read back. The accounting half of a row is an
`Option`, and a kind that has none is **unmetered**: it launches, runs and lands its work,
and simply produces no ledger line. That is a stated cost rather than a refusal — see
[Accounting is
optional](agents.md#accounting-is-optional-and-its-absence-is-a-cost-not-a-refusal) for the
whole of what it gives up, and `spoolway agent verify <kind>` for the per-kind answer. **Every
launchable kind is metered today**, so nothing currently occupies that state.

A kind spoolway does not yet know how to *launch* is a different thing: it has no argv
template at all, so a profile naming one is refused before a lane ever starts.

## Reading it

```
spoolway spend                                  # this project, by step
spoolway spend task                             # what each task came to
spoolway spend lane                             # one row per lane, newest last
spoolway spend project --all                    # everything, one row each
spoolway spend group --project webshop          # one named project
spoolway spend --month 2026-08                  # a calendar month, local time
spoolway spend --since 2026-06-01 --until 7d    # dates and durations, either end
spoolway spend month --all                      # what each month came to
```

`eval --by` still works as a deprecated alias for the same table, with a note to stderr
pointing here.

```
STEP          LANES         IN        OUT    CACHE R    CACHE W    COST USD      WALL
implement         4      52.2k      15.4k     794.4k          0           0       49m
review            4        536     222.1k     27.22M     568.3k       24.85       53m
pipeline         17     118.4k     339.9k     36.57M     683.2k       31.12     3h29m

total                    131.1k     399.7k     64.21M     811.5k       43.27
```

One table and one total. The rows are the ledger grouped by the named cut; the last row, `total`,
is the sum of every row above it — the same columns, all of them filled, with no second
population left to hold apart. Every line of spend that is not a lane is skipped wherever this
table is read, so it shows lanes only.

### Reading a row

`IN`, `OUT`, `CACHE R` and `CACHE W` are the four priced token classes, and they are
disjoint. `WALL` is how long lanes were open, not model time.

A cost of `—` means the model resolved to nothing in any of the price tables — see
[Pricing](#pricing). A group where only some of its lanes could be priced still prints the
plain number it priced, a floor rather than a total; the report's own footer names the model
responsible. A line that spent no tokens at all — the zero-token line a session is enrolled
with, or one of Claude Code's own locally generated messages (a spend-limit notice, an API
error, an interrupt) — has nothing missing from the total either way, so it never turns a
group into a floor on its own.

### Grouping

The cut is a plain positional argument: `task`, `group`, `step`, `model`, `project`, `month`,
`lane`. Each answers a different question — `step` says where the pipeline's spend goes,
`model` what each one costs to run, `task` and `group` what a piece of work came to. `lane` is
the one cut that prints a different table entirely: one row per lane, newest last, instead of a
grouped summary.

It defaults to `step`, or to `project` when more than one project is in scope.

Under the global `--json`, `spoolway spend` does not honour the cut at all: it dumps the
matching ledger entries themselves, raw and ungrouped, whatever cut was named. A script wanting
the grouped figures reads the plain table, or `--csv`, instead — `--json` and `--csv` are
refused together, since they are two different exports of the same rows. In `--csv`, a name
that holds a comma, a double quote or a newline is double-quoted the RFC 4180 way, so a
project directory called `foo, bar` does not shift every column after it.

### Skill sessions

The lanes are not where all the money goes. Planning, queueing and reshaping a prompt happen
in **your own** session, and that session's transcript already says which skill was running
when each turn was spent: Claude Code writes a slash command as a user record, ahead of the
assistant turns it spent. spoolway partitions the transcript on those markers and banks each
stretch as an ordinary ledger line carrying a `skill` label — the command's own full name, so
`/spoolway-plan`, `/my-plan` and `/code-review` are recorded as themselves rather than folded
into one bucket the moment they are not one of spoolway's own.
`interactive` is left for the one stretch no command ever names: before the first marker in the
transcript.

Which session that is comes from the environment: `$CLAUDE_CODE_SESSION_ID` for claude,
`$CODEX_THREAD_ID` for codex. Both, when there are both — a codex session started from inside
a claude one is really inside both, and each is enrolled and banked on its own. A codex
rollout carries no command envelope spoolway reads, so a codex session banks entirely under
`interactive`: the spend is counted, the skill that spent it is not.

No skill has to cooperate. Nothing is added to a `SKILL.md`, nothing has to be remembered at
the end of a session, and a skill written next month is counted from its first run.

Two halves make that work, and they are deliberately apart:

| Half | What it does |
|---|---|
| **Enrol** | Any spoolway command run in an interactive session banks what it has spent so far. That line is how spoolway learns the session exists at all — and a session whose transcript carries no turn yet is enrolled with a zero line rather than skipped, because a session nothing enrols is a session nothing ever comes back for. codex writes a turn's usage when the turn *ends*, so the first command in one really does read an empty rollout |
| **Sweep** | Reading the ledger — `spoolway eval` or `spoolway spend` — re-reads every session it names, and appends what has arrived since. So a plan you never queued, and the hour of conversation after your last command, are still counted |

Both bank only the **delta** since that session was last banked, per skill — so reading twice
with nothing in between appends nothing, and the ledger totals to the session rather than to a
multiple of it. This does mean reading the ledger writes before it prints: idempotent, and to
this project's ledger only, never to another project's under `--all`.

A skill line is counted in **distinct sessions**, not rows: one session banks a line every
time it is swept. It carries no run, no outcome and `wall_s: 0` — a session's open hours
measure how long you had the window up, not model time. That is why neither `spoolway eval`
nor `spoolway spend` shows skill spend in its tables: both read lanes only, and a skill session
is not a lane.

Every command's real name reaches the ledger, and every name is counted in full — no list
narrows what is banked.

What it cannot do:

- A marker says where a skill *started*, never where it ended, so a session that carries on
  after one is charged to it until the next marker arrives.
- Skill labels are claude's alone. A codex session is counted in full, but every line of it
  reads `interactive`: its rollout carries no command envelope to partition on.
- `pi` is left out entirely — it exports no session id, so an interactive session of it goes
  unaccounted. If it ever does, the `session_env` line of its row in `agent::ADAPTERS` is the
  only thing to change.

  That is a gap in *enrolment*, not in metering. Every shipped kind is metered and `spoolway
  eval` is a view over the ledger with no notion of kind, so every kind is evaluable. What only
  a cloud model carries is a **price**: the vendored litellm table prices what the providers
  publish and nothing else, so a local model reads as tokens at zero cost — by design, not by
  omission. `pi` is the one kind you point at your own server; `claude` and `codex` are the two
  that arrive priced. A project that wants its local model priced
  writes a glob in `[models]`, which already wins over the vendored table — see
  [Pricing](#pricing).
- A session that never runs a single spoolway command is never enrolled, and never counted.
- A lane is itself an agent session, so a lane's own spoolway commands bank nothing: the
  `SPOOLWAY_STEP` in a lane's environment is what tells the two apart.

### Settled lanes

A lane's transcript can keep growing after the dispatcher has torn the lane down. A tool
result can land late. A person's interrupt can leave turns behind that the agent only writes
out afterwards. The single line banked at teardown does not cover those turns.

Reading the ledger catches them up. `spoolway eval` and `spoolway spend` sweep every settled
lane session the ledger names, and append one line for whatever the transcript has gained
since that session's last banked line. The new line copies `task`, `step`, `pipeline`,
`agent`, `plan`, `run`, `trial` and `round` from the lane's most recent line, so the
recovered spend lands in the same `spoolway eval` row the lane's own turns did. It carries no
`outcome`: the turns arrived after the lane reported, or after it was killed without
reporting, so nothing judged them.

This is gated on the transcript's mtime. A settled lane whose file has not moved since its
last banked line is not read at all, because otherwise every ledger read would re-parse the
largest file in every finished run. A tie is read rather than skipped: a line banked in the
same second the last turn landed is no proof nothing came after it.

A lane still in flight is left alone. Any session named in `lanes.json` is the dispatcher's
to bank at teardown, so the sweep skips it. The dispatcher diffs a lane's spend against a
ledger snapshot it took once at the start of the pass, so a catch-up line slipped in behind
it would be counted a second time.

### Windows

`--since` and `--until` each take a duration back from now (`4h`, `2d6h`), a local date
(`2026-08-01`), or a whole month (`2026-08`). `--month` is the shorthand for the `--since`/
`--until` pair that bounds one calendar month.

Dates and months are read in **your timezone** rather than UTC, because a person asking what
August cost means their August. `--until <date>` includes the whole of that day, and
`--until <month>` includes the whole of that month.

## Pricing

```toml
[models."claude-opus-5"]
context_window = 1000000
input = 5.0
output = 25.0
cache_read = 0.5        # 0.1x input
cache_write_5m = 6.25   # 1.25x input
cache_write_1h = 10.0   # 2x input — a longer-lived cache costs more to write
session_reuse_idle = "5m"   # how long a carried session may sit; see below
```

Prices are per million tokens, keyed by a glob over the model name — the same table that
holds `context_window` and `session_reuse_idle`, since all of them are facts about the model
rather than about a launch profile.

`session_reuse_idle` is the newest of those and the one with the least obvious home, so: it is
here because a prompt cache belongs to whoever *serves* the model, and most agent CLIs are
harnesses that can be pointed at any provider. No fact about `pi` or `codex` could say how
long a cache entry lives, because the same binary might be talking to Anthropic
or to a llama.cpp socket. The model can. Leave it unset and a carried session under this model
is never refused for its age.

Set it only for a **hosted** model. A local one has no cache lifetime to state — a llama.cpp
slot is held until something evicts it, not until a clock runs out — so a number here would
be invented, and an invented expiry throws away a session that was still perfectly usable. See
[Cache warmth is a model's fact](agents.md#cache-warmth-is-a-models-fact). Renamed from
`cache_ttl`: a serde alias keeps an existing config parsing, and the rename is written back on
the next save.

`context_window` is read twice: the planning skill reads it to judge whether a task fits a
session of that model, and `agents.<profile>.session_reuse_ctx` takes its percentage of it,
to judge whether a prompt's earlier session on this task is still small enough to carry over
on a step whose `session:` asks for it — see [A step that carries its own
session](dispatcher.md#a-step-that-carries-its-own-session). A model with no window set here
never carries a session over that way, only ever opens fresh.

Set a price non-interactively with:

```
spoolway config set models.'<model-glob>'.input <usd per 1M>
```

The table is **empty by default** — spoolway does not know what you run — but it is not the
only place a model can be priced from. Resolution is three tables deep, checked one model at a
time: behind the project's own sits a machine-wide table at `~/.spoolway/model-prices.json`, a
refreshed copy of litellm's price map distilled to the same six numbers, and behind that a
built-in table vendored from the same map into the binary. Each price table is matched by the
model's exact name when nothing in `[models]` matched it by glob, and a project's own glob
still wins over either of them. A step naming `claude-opus-5` is priced from the first table
that knows it the moment it runs, with nothing to configure; a step naming a local model
nobody has published a price for still resolves to nothing, and is reported as unpriced rather
than folded into a total as zero — a model in none of the three tables has an unknown cost, not
a free one. `spoolway models` lists every model this project's pipelines name, its window, its
rates, its own `slots` and `exclusive` (see [`[models."<glob>"]`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is)),
and which of the three tables answered — or `unknown`, where none did. It closes with the age
of whichever table actually answered — the machine-wide refreshed file when it parses, the
built-in otherwise — naming its `generated` date, its age in whole days, and `spoolway models
refresh` as the way to make it current.

Renamed from `[pricing]`, which held the same five rates without the window; an old `[pricing]`
table is read into this same field and written back under the new name.

The built-in table is a vendored file, not a live lookup: it is compiled into the binary and
never fetched at runtime. The one network edge in this file is refresh, which shells out to
curl — see [Where the built-in table comes from](#where-the-built-in-table-comes-from).

The refreshed table in between is optional user-state, written by `spoolway models refresh`
and read here only by name. An absent, unreadable, or unparseable file is silently
skipped, so a partial refresh never erases what the built-in table still holds for a model it
doesn't list and a broken file never breaks `spoolway models`.

### Who prices what

- **pi** prices its own transcripts, and reports zero for a local model — which is the whole
  thesis of the design stated as a measurement rather than a claim.
- **Claude Code** records no cost, so the pricing table answers for it. It also writes some of
  its own messages — a spend-limit notice, an API error, an interrupt — as an assistant turn
  under the model name `<synthetic>`, with an all-zero `usage` block. Nothing prices that name,
  and a turn under it never overwrites the lane's real model, token totals, or last-turn size:
  a lane whose transcript happens to end on one is still priced and sized by the turn that
  actually answered.

### Where the built-in table comes from

`assets/model-prices.json` is litellm's `model_prices_and_context_window.json` (MIT licensed),
distilled from roughly 2,200 priced chat models down to the six fields spoolway uses, and
compiled into the binary. `spoolway models refresh --vendor` is how it is refreshed: it fetches
the upstream file through curl, distils it to the same six fields, and writes
`assets/model-prices.json`; the diff is reviewed and committed like any other vendored
dependency. There is no schedule and no check on startup. A stale table is never fetched for
and never fails a check; the only thing it does is earn a `spoolway doctor` note once it passes
`max_age_days` — advisory, never a failure. That is the whole of what the binary calls out about
a stale table, and it is the price of the binary never calling out at any other time.

### Why cache writes are two rates

A cache write is priced by the cache's lifetime: a five-minute write costs 1.25× the input
rate and an hour costs 2×. Folding both into one rate understates a real transcript by about
a third, which is why they are separate keys.

Whether a carried session is still worth resuming is a separate question from price now,
answered off the store's own age rather than off this split — see [Cache warmth is a model's
fact](agents.md#cache-warmth-is-a-models-fact) and [A step that carries its own
session](dispatcher.md#a-step-that-carries-its-own-session).

## The version a lane ran under

Each line also records a fingerprint of the tracked `.spoolway/` configuration, the commit
that last touched it, and what the lane reported — which is what makes it possible to ask
whether an edit to a prompt made the work cheaper. See [Comparing versions](eval.md).

## One ledger per project

Each ledger lives in its own project home, `~/.spoolway/<project>/usage.jsonl`, so projects
never share a file and there is nothing to coordinate. Running `spoolway eval` inside a
project — or pointing it at one with `-C` — is already scoped to it.

The cost of that is that nothing knows where the other ledgers are. So setup and dispatch
note the project root in a small index under your state directory, and `--all` reads every
ledger it lists.

That index holds **no usage of its own**: delete it and the worst that happens is `--all`
forgets a project until its next dispatch.

## What this is for

The design goal is to spend as little cloud inference as possible: local lanes do the work,
one cloud call reviews it, and the dispatcher itself uses no model at all. Every scheduling
decision is a lookup — a stage, a lane status, a counter, a timestamp.

The ledger is how you check that the goal is actually being met on your machine, with your
models, rather than taking the claim on trust.
