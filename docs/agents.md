---
domain: agents
covers: ["src/agent.rs", "src/quota.rs"]
---

# Agents and models

An agent profile says which agent binary runs, how many at a time, and under what limits.
Steps name a profile; profiles name a kind. Pointing a step at a different CLI is a
configuration edit, not a code change. Which *model* runs, and how hard it thinks, are not
part of a profile — they are the step's, covered under [Which model a step actually
runs](#which-model-a-step-actually-runs) below.

## Profiles

Profiles live in the config file under `[agents.<name>]`, and a pipeline step refers to one
by name. Three ship:

| Profile | Kind | |
|---|---|---|
| `pi` | `pi` | referenced by the built-in pipelines |
| `claude` | `claude` | referenced by the built-in pipelines |
| `codex` | `codex` | there to be pointed at |

**Each is named after the kind it runs**, and the two columns saying the same thing is the
point rather than an oversight. They were once named for a *role* — `local` and `cloud` — on
the reasoning that a step should say where its work runs and a profile should be free to
change which binary does it. What hollowed that out is everything a profile has since stopped
carrying: `model`, `context_window` and `args` all moved to the step or to `[models]`, and
what is left is the binary and the limits around it. A profile is now so nearly *just* its
kind that naming it anything else was a layer of indirection with nothing inside.

Two consequences worth being clear about. Swapping the binary a step runs on is an edit to
that step's `agent:` now, not a one-key change in the config that no pipeline sees. And these
are still ordinary profile names, not kinds spoolway resolves — `[agents.fast]` running
`kind = "pi"` is as valid as it ever was, and an existing config naming its profiles `local`
and `cloud` keeps working untouched, since a step is checked against the profiles the config
defines and nothing else.

`codex` is referenced by no shipped step. It ships anyway so that moving a step onto it is an
edit to that step's `agent:` rather than a profile somebody has to write first — the argv, the
permission flag and the session home are already settled per kind in `agent::ADAPTERS`. `codex`
runs against a local server and carries the same limits `pi` does.

Which steps run on which profile is the pipeline's business now — see
[Pipelines](pipelines.md) — not a fact this file states about the profile.

### Every profile key

| Key | Default | Meaning |
|---|---|---|
| `kind` | `pi` | Which agent binary this is. Decides the argv template, the headless flags, the permission-mode flag, and how (or whether) it carries a step's effort |
| `concurrency` | **absent** on every shipped profile | Most lanes of this profile running at once. `0` is unlimited. A cap on the harness — for a local model the count belongs to the model, as `models."<glob>".slots` |
| `session_reuse_ctx` | `0` — off | How large an earlier session may be, as a percentage of the model's context window, before a `session: true` step opens fresh instead of carrying it over. `0` — the default — never refuses reuse on size; a nonzero value is `1..=100`, judged on the last turn alone |
| `session_blocked_ctx` | `0` — off | How large a *running* lane's last completed turn may get, as a percentage of that window, before the dispatcher stops the lane and blocks its task. Must be above `session_reuse_ctx` when both are set — see [`[agents.*]`](configuration.md#agents--who-runs-a-step) |
| `quota_ceiling` | `0` — off | How much of its kind's own account-wide quota may be spent, as a percentage of either window, before a pass starts no new lane of this profile and parks every candidate task instead. 1..=100, or `0` for off. Holds new launches when the reading is unavailable, invalid, stale or expired — see [Reading a kind's quota before a lane starts](#reading-a-kinds-quota-before-a-lane-starts) |
| `permission_mode` | kind's own first, strongest-unattended mode; **absent** on a kind with none | Whether this kind's lanes stop and ask about a tool call. Holds the mode a lane is actually started with — `claude` ships `"auto"`, `codex` ships `"never"` — never a placeholder for one; a blank is refused by `spoolway config set` and never reaches `Config::load` |

A profile no longer carries `model`, `context_window`, `args`, `sandbox`, `sandbox_extension`,
`env` or `session_reuse_uncached`. A profile running several steps could only ever name one
model for all of them, which is exactly the problem naming it per step fixes; every flag `args`
ever held was spoolway addressing its own CLI with paths and ids only spoolway computes, not a
project's to hand-tune — see [The argument template](#the-argument-template); the two sandbox
keys went with the sandbox itself, below; `env`, the one lever that let two profiles of a kind
differ, went because every kind it fronted already has a config file of its own for that job;
and `session_reuse_uncached` went the way [Cache warmth is a model's
fact](#cache-warmth-is-a-models-fact) describes — the horizon it argued about is
`models."<glob>".session_reuse_idle` now, and leaving that unset already says what setting this
to `true` used to. Every retired key still parses in an old config and is dropped on the next
save.

### spoolway names none for you

Every agent step in a pipeline must name its own `model:` — spoolway names none for you, so
`pipeline check` and `doctor` refuse a step whose model is missing or blank, and a lane
refuses to start rather than launching an agent with an empty model flag.

This is a deliberate refusal to guess. spoolway does not know what your server serves, and a
wrong guess would silently send local work to somebody's API.

## Agent kinds

A kind is **one row** in spoolway's adapter table, and the row spans both halves of what
spoolway knows about that CLI: how it launches — which permission modes it accepts, which
argv (if any) carries a step's `effort:`, which flags let it run headless — and how its
spend is read back afterwards. Every clause is settled by running the binary, not by reading
its help output, because a resume flag guessed wrong loses a lane's context silently rather
than loudly.

| Kind | Permission modes | Effort | Headless | Accounting |
|---|---|---|---|---|
| `pi` | none — pi has no approval prompt to switch off | none — `--thinking` takes a token budget, a different axis from a named level | yes | `~/.pi/agent/sessions`, `<prefix>_<id>.jsonl`, `Pi` |
| `claude` | `auto` (default), `acceptEdits`, `dontAsk`, `bypassPermissions`, `plan`, `manual` | `--effort` | yes | `~/.claude/projects`, `<id>.jsonl`, `AnthropicApi`, `$CLAUDE_CODE_SESSION_ID` |
| `codex` | `never` (default), `on-request`, `untrusted` | `-c model_reasoning_effort=<level>` | yes, via `exec` | `$CODEX_HOME/codex`, `<id>/sessions/**/*.jsonl`, `Codex`, `$CODEX_THREAD_ID` |

Every kind launches, runs headless, resumes, attaches and is **metered**; see [What the
probes found](#what-the-probes-found) for what each clause was settled by. A kind with no
argv template cannot be launched at all, and a profile naming one is refused.

**A kind's name and its executable are the same string, for every row.** `Adapter::binary` is
`None` on every row today — kept as a distinct field rather than folded into `kind` because a
future row may again name a CLI different from its kind. `spoolway agent list`'s `BINARY`
column, and `headless.rs`'s argv, resolve the program to start through this rather than through
`kind` directly, so the two could drift apart in exactly one place if a row ever needed it.

Accounting is still an optional half of a row — see [Accounting is
optional](#accounting-is-optional-and-its-absence-is-a-cost-not-a-refusal) — but no shipped
kind occupies that state today.

### Telling a spent quota from a dead lane

A row can also carry `usage_limit`: the exact phrase this kind's own CLI leaves in a pane when
it has hit an account-wide usage limit and stopped making progress on its own. Only `claude`
declares one today, `"Usage limit reached"`, read verbatim off a real transcript rather than
guessed at. A kind with no `usage_limit` established, `codex` among them, leaves its lanes on
the ordinary settled/reminder path; `Adapter::is_usage_limit` answers `false` for every tail on
such a kind.

**The pane is left alone when this phrase shows up.** The agent resumes its own turn once the
window resets, so tearing the lane down would throw away work that is going to continue by
itself. The dispatcher parks the task instead: it writes the next recheck to `parked_until:`
and stamps the continuous hold's fixed start in `parked_at:`. The board renders the increasing
elapsed age from that start while the lane keeps its pane, session and worktree and the reminder
loop stops nudging it. The park runs to an observed exhausted window's reset, or uses a separate
quota-recheck backoff from one minute to one hour. Repeated holds do not append duplicate log
entries. See [Restarts, laps and escalation](dispatcher.md#restarts-laps-and-escalation).

### Reading a kind's quota before a lane starts

A row can also carry `quota`: where this kind's own CLI leaves its account-wide usage
percentage. `claude` and `codex` both declare one today. Nothing here goes over the network;
the figure is something the agent already wrote to disk.

The two kinds write it in different places, so the string on the row means a different thing on
each. `claude` names a **file** relative to the home directory, `.claude.json`, whose
`cachedUsageUtilization.utilization` object holds a `five_hour` and a `seven_day` entry.
Each is an integer `utilization` percent and an ISO `resets_at`; `fetchedAtMs` on
`cachedUsageUtilization` dates the pair. The reader also accepts the flat layout with both
windows directly under `cachedUsageUtilization`. A malformed nested layout is rejected,
even if a flat pair is also present.

`codex` names a **directory** instead, `sessions`, because codex writes its figure per session
rather than to one cache. Every `token_count` event in a rollout carries a `rate_limits` object,
with a `primary` window and a `secondary` one. `crate::quota::read` walks the rollouts under
both homes newest-first and takes the newest one that carries a reading; a rollout with both
windows null is passed over for the next-newest. The last `token_count` event's `rate_limits` is
what is read out of whichever file wins, and the event's own `timestamp` is what dates the
reading, standing in for claude's `fetchedAtMs`.

**Codex is read from two places, and the newest rollout across both wins.** The managed homes
are the `<state_root>/codex/<session>/sessions/` directories a dispatched lane writes into. Each
of those directories is itself the `$CODEX_HOME` spoolway handed that lane, so the probe does read
per-lane `CODEX_HOME` values. The other home is `~/.codex`, where an interactive session or a run
against a local endpoint writes. Both are walked: every `.jsonl` under a session's `sessions/`
tree is a candidate, not just the newest file in it, so a real reading under an older null one is
still found.

The ambient `$CODEX_HOME` this process would otherwise resolve is not a third home. A dispatcher
started inside a codex session inherits it pointing at a managed home the scan already covers, so
honouring it would make the reading depend on the shell the dispatcher launched from rather than
on the account.

A rollout in `~/.codex` comes from a run spoolway did not start, and the case that guards
against it — a session settled against a local endpoint — writes `rate_limits` with both windows
null, which is never a reading here. That null is skipped rather than allowed to win on recency,
so it cannot blank out the account's real figure from the other home; when no rollout anywhere
carries both windows the result is `Miss::NoReading`. Reading only the managed homes deadlocks
the queue: only a codex lane writes a managed rollout, and the gate reading it holds every codex
lane, so a reading that aged out could never be replaced.

The reading gates a launch through [`quota_ceiling`](#every-profile-key). At or above the
ceiling on either window, a pass starts no new lane of that profile, writes `parked_until:`
from the window's own `resets_at` on every candidate task, and stamps `parked_at:` on the first
hold. Rechecks preserve that start, so status can keep counting the elapsed age. The five-hour
window is checked first, because it is the one that resets soonest. Codex's `primary` and
`secondary` windows are carried in those same two slots. Tasks whose step names a different
profile are staffed in the same pass, untouched.

**An enabled ceiling requires a trustworthy reading.** Missing probes, unreadable or
malformed files, readings older than five hours, and expired windows hold new launches.
Codex's all-null windows also hold: they carry no account quota reading. A half-null pair
is malformed. The task says `quota unavailable`, and its status log points to
`spoolway agent verify <kind>` for diagnosis. Rechecks start after one minute, double to
an hour, and survive dispatcher restarts; a fresh reading permits admission at the next recheck.
Spoolway only reads these files: the agent must refresh them. With no writer, the hold
continues until the reading is refreshed or the ceiling is disabled. Existing lanes retain
their sessions, and profiles with `quota_ceiling = 0` take no reading.

The ceiling is an admission threshold, not a completion guarantee. Concurrent lanes and
sessions outside spoolway share account quota; no quota is reserved for their remaining work.

`spoolway agent verify` prints each kind's quota clause, so a person can see which of those
cases they are in:

```
claude   quota  ~/.claude.json cachedUsageUtilization
                five_hour 61% resets 14:00 · seven_day 16% resets 09-11 04:00
codex    quota  newest rollout under either codex home,
                last token_count event's rate_limits
                five_hour 2% resets 14:00 · seven_day 6% resets 09-11 20:33
pi       quota  no probe established — an enabled quota ceiling holds new launches;
                its usage limit is not detected either
```

Both kinds render their windows the same way: the window's own key (`five_hour`/`seven_day`)
and an absolute local reset time. A reset within the day shows a bare clock; a reset on another
day shows `MM-DD HH:MM`, because the line already carries "resets" and a year nobody asked about
would only crowd it. The account behind codex's reading is not necessarily on this machine's own
timezone, so the reset is an absolute local time rather than a countdown.

The clause never fails the command. `spoolway doctor` says the same thing from the other side:
it names a profile that sets `quota_ceiling` on a kind with no probe, where new launches stay held until the ceiling is disabled or a supported kind is used.

### Telling an interrupted turn from a finished one

Herdr's own status vocabulary — `idle`, `working`, `blocked`, `done`, `unknown` — cannot tell
a turn a person cut off with Escape from one that finished on its own: both land as `done`.
The difference only survives in the transcript, so a row can also carry `abort_marker`: the
shape of the one record that kind's own transcript ends in once an Escape lands mid-turn.
`claude`, `codex` and `pi` each declare one, read verbatim off a real transcript rather than
guessed at. `crate::usage::last_turn_aborted` reads a session's transcript against its kind's marker and
answers whether the last record in it is that kind's own interrupt shape. The dispatcher calls
it on every settled lane, ahead of the reminder loop — see [A lane that settles without
reporting](dispatcher.md#a-lane-that-settles-without-reporting).

### Two ways to pin a session

Every reading in [cost accounting](cost.md) starts from the same question: *which transcript
belongs to this lane?* The answer has to be something spoolway chose, never a guess from a
working directory and a modification time. There are two ways to arrange that, and which one
a kind uses is the shape of its `store` clause.

**By the filename.** pi and claude both accept a session id, so the dispatcher mints one per
lane, passes it through as `{session_id}`, and afterwards looks for the transcript whose name
carries it. `FileShape::Exact` and `FileShape::AfterUnderscore` are the two spellings of that.

**By the directory.** codex mints its own id and refuses any other — `codex exec resume <a
fresh uuid>` fails outright and `-c session_id=…` is rejected as an unknown field. So nothing
in the filename is spoolway's to match. What *is* spoolway's is the directory: the lane gets a
state directory named after the id spoolway minted, and the agent is handed a path into it.

That is the `home` clause — `dir`, the variable it goes in, and what inside it the variable
names:

| Kind | Variable | What it moves | Seeded with |
|---|---|---|---|
| `codex` | `$CODEX_HOME` | the whole state tree — transcripts, credentials, provider config | `auth.json`, symlinked back to the real home; `config.toml`, copied and then written into |

The clause sits on the *adapter*, not on the accounting row, because pinning a session and
reading its spend are separate things and a kind may need the first without the second — a
home that could only be declared beside an accounting row would leave such a kind resuming
whichever session ran last on the machine, which with two lanes up is the other lane's.

Pinning by directory is what makes a resume that names nothing honest. `codex exec resume
--last` on its own means "the most recent session on this machine", which is emphatically not
this lane's. In a home holding exactly one session, it is. A test holds the two halves
together: a kind that resumes without naming a session must declare a home, and a lookup that
reads out of one — a `FileShape::OwnHome` transcript — must have one to look in.

Seeding matters for the home that moves everything: `CODEX_HOME` takes the credentials and
provider config with it, so spoolway links them back to the real home rather than copying
them. A login refreshed in the real home stays good, and nothing spoolway made holds a stale
token. The link is a symlink on Unix and a symlink or same-volume hard link on Windows. A
plain copy is the fallback only on a platform that will make neither, and there a rotated
token does go stale in the session home.

One seed is the exception, and it is the `trust` clause beside `seed`. codex will not start
in a directory nobody has said it may work in. Its interactive frontend opens on *Do you
trust the contents of this directory?* and waits for a keypress, which under a multiplexer
is a lane stalled on a question with nobody there to answer it. The answer lives in
`config.toml`, so spoolway writes that file rather than linking it: the real one's contents
first, then a `[projects."<worktree>"]` table saying `trust_level = "trusted"`. Appending
through a link would write a lane's worktree into the person's own config, and every
worktree after it too.

Three things about that were settled by running codex-cli 0.153.2. The entry is read from
the file on disk and only from there — the same table passed as `-c
projects."<dir>".trust_level=trusted` is parsed and ignored, so there is no flag route. Only
the working directory is asked about, not the directories granted with `--add-dir`. And
`codex exec` never asks at all, which is why the headless checks spoolway already runs came
back green while the lanes a person watches did not — see [Asking whether a kind actually
works](#asking-whether-a-kind-actually-works).

The entry is a key that gets *set*, not a table appended to the end of the file. The file may
already speak about the directory: `codex exec` writes a `projects` table for every directory
it runs in, so any project root that has ever had a headless turn in it already has one. A
second table with the same name is a duplicate key, and codex answers that by refusing to load
the config at all — a lane worse off than one that only had a question to answer. A path is
also a poor thing to splice into a line, since it holds dots and, on Windows, backslashes.

A config spoolway cannot parse is copied through untouched. The lane then reports codex's own
complaint about the person's file, which is the thing they need to see, rather than running
against a config spoolway invented with no provider in it.

### codex asks about its own updates too

On startup codex checks for a newer release of itself, and a newer one stops a lane dead:

```
✨ Update available!
0.153.4 -> 0.153.6
› 1. Update now (runs `npm install -g @openai/codex`)
  2. Skip
  3. Skip until next version
Press enter to continue
```

Nobody is there to press it, so the lane never reaches `working` and the launch is reported
as a failure — `agent is no longer running in the target pane`. The dispatcher retries on the
next pass, so a run survives this, but it costs a lane.

Somebody pressing it is the worse outcome. Option 1 replaces the codex binary while other
lanes are running it, creating the same live-binary replacement hazard spoolway guards against
for its own dispatcher, arriving through codex instead.

So a lane is launched with `-c check_for_update_on_startup=false`. Unlike the trust table,
this key really is read from the merged config, so it is a flag rather than something written
into the lane's file — established by running it, with a home whose `version.json` named a
newer release. The dialog came up with the check on and did not with it off. The key is
accepted under `--strict-config` and a near miss, `check_for_update_on_startupp`, is refused,
so the name is the binary's rather than a guess. Only the lane's copy of codex stops asking;
a person's own codex reads a config spoolway never touched and still tells them.

The per-session home itself does not outlive the lane. Once a lane has been banked and its
task archived, the dispatcher removes the home it made, so seed links and any copies go with
it. A lane held at `blocked` keeps its home, since `spoolway resume` still needs the
transcript inside it.

**And the session nobody pinned.** All of the above is a *lane's* session. Your own — the one
you plan in — was already running when spoolway was invoked inside it, so there is no home
spoolway made and no id it minted: the id arrives from the environment instead, and the
transcript is in the agent's own home. So `FileShape::OwnHome` resolves two homes, and what
picks between them is whether spoolway ever made one under this id. It did: a lane, read from
the home spoolway named. It did not: a session of the agent's own, read from `$CODEX_HOME`
(or `~/.codex`), where the tree holds every session it ever wrote — so there the id *is*
matched in the filename, at the end of it, `rollout-<ts>-<id>.jsonl`. Asking that question
first also keeps a lane whose first turn has not landed yet from falling through to a walk of
every session on the machine.

A store says where a kind's records are and how they are enumerated — a separate axis from
[`Format`], which says how one record reads. There is one kind of store today: `Transcript`,
one JSONL file per session, found by `FileShape` and read a line at a time. Everything above
the store — running totals, last-turn size, the store's own age, the skill segments — works on
the records it yields and never learns anything about the container beyond that.

### Cache warmth is a model's fact

A carried session is not resumed forever. `carried_session` weighs it against two bounds: how
large it has grown, and how long its store has sat since anybody touched it. The second one
used to be a guess at whether a prompt cache was still warm, timed off a lifetime nothing
publishes; it is a plain age check now, against a horizon a project states —
`models."<glob>".session_reuse_idle`, matched by the same glob that prices the model:

```toml
[models."claude-*"]
session_reuse_idle = "5m"

[models."gpt-*"]
session_reuse_idle = "10m"     # OpenAI's automatic caching, as your own measurement finds it
```

**A cache belongs to whoever serves the model, not to the harness that asked**, which is why
this lives here and not on the agent profile. `pi` and `codex` are harnesses — the
same binary talks to Anthropic, to an OpenAI-shaped endpoint, or to a llama.cpp socket on the
next port — so no fact about the harness could ever have said how long a cache entry lives,
and one profile serving several vendors' models could only ever have named one lifetime for
all of them.

Age is read off the store itself — `touched_at`, the same mtime the reminder loop already
takes, so a store with no transcript at all is covered without being taught anything new.
Unset, and a carried session under that model is never
refused for its age; unreadable, the same. Neither is a guess at how long a provider's cache
"usually" lasts — it is the honest absence of an answer, and it refuses nothing on its own.

**Do not set this on a local model.** A llama.cpp cache is not on a timer — it holds a slot
until another request needs it or the server restarts — so there is no duration that says
when it goes away, and any number written here would be a fiction the dispatcher then acts on.
A `session_reuse_idle` that has quietly expired turns into a refused session reuse and a
conversation re-sent from scratch, which costs a local run its whole reason for carrying a
session. Leave it unset, and the session is carried however long it has sat.

The setting is for the *other* thing a harness does — being pointed at a hosted provider,
whose cache really does expire on a clock. `pi` takes `anthropic/…` as readily as a local
socket, and that is the lane this exists for. Renamed from `cache_ttl`: a serde alias keeps an
existing config parsing, and `spoolway doctor` names both the old spelling and the retired
`session_reuse_uncached` beside it, so a project finds out rather than carrying a dead key
silently.

### Accounting is optional, and its absence is a cost, not a refusal

The two halves of a row are not equally load-bearing. A row with no `args` cannot be
launched — there is nothing to launch it with, and that refusal stands. A row with no
**accounting** launches perfectly well. It simply runs unmetered, and that is a finished
state rather than a gap.

**No shipped kind occupies that state today.** An earlier row did, on a finding about its
*storage medium* that turned out to be about a container rather than a capability — the row
gained an accounting half and nothing else in the design had to move. A kind that genuinely
cannot be read back is still a legal, finished row, and this is what it would cost:

| What is lost | What happens instead |
|---|---|
| Ledger lines | Nothing. `spoolway spend` never sees a lane of that kind — the one consumer with no fallback |
| Session reuse | Every step opens a fresh session: `carried_session` misses, which it already handles |
| Silence detection off the transcript | The dispatcher hashes the pane instead, which it already falls back to. Fine under a multiplexer, weak headless |
| The lane itself | Nothing. It starts, runs, reports and lands its work |

Nothing anywhere refuses a kind for being unmetered — not `render_args`, not lane start, not
`doctor`. `doctor` reports such a profile as a **note** naming exactly what it gave up, and
the check still passes: a project running an unmetered kind on purpose should not have a
permanently red doctor. What it should not have is a silent one.

### Asking whether a kind actually works

Two commands, over the whole adapter table rather than over the profiles this project
configures — because the kind you want to ask about is usually the one no profile names yet:

```
spoolway agent list              every kind: launch state, accounting state, binary
spoolway agent verify claude     one kind, clause by clause
spoolway agent verify pi --live --model <name>
                                 …plus the readings only real turns can settle
```

`verify` reports each clause separately — binary, launch row, args render, how the session is
pinned, resume rewrite, accounting row or its absence, transcript directory — and its exit
code follows the launch half alone. Without `--live` nothing is started and nothing is spent,
so it is safe against a kind you have never authenticated.

`--live` runs a turn, then a resumed one, and reads back everything a lane's start depends
on:

- **The turn ran**, and what came back.
- **The prompt reached the turn.** The check's prompt asks for a token back, and the reply
  is searched for it. This is the one clause no file can answer: every kind takes its prompt
  a different way — a flag, a config override, a whole config document in the environment —
  and each of those can be accepted by the binary and then ignored. Missing, it *warns* rather
  than fails, because a small local model that read the instruction and did not comply looks
  exactly the same from here.
- **The session landed in the home spoolway made**, for a kind pinned by directory. This is
  what catches a relocation variable the binary quietly ignored — which would otherwise show
  up only as two lanes continuing each other's conversation.
- **The transcript readings**, for a metered kind: tokens, last-turn size, store age,
  mtime, running totals. A kind can carry a perfect accounting row and write a transcript in a
  shape the parser no longer recognises; every reading comes back empty, every lane of that
  kind quietly goes unaccounted, and nothing goes red. A reading that fails *does* fail the
  command — an absent accounting row is legal, but a declared one that does not read back is
  wrong.
- **The resumed turn continued the same session.** The second turn is composed exactly as a
  lane's second turn is — the same rewrite, `codex exec resume --last`, claude's
  `--session-id` → `--resume` swap — and the
  *store* is asked whether it grew rather than the model whether it remembers. A resume
  spelling was settled against one
  version of a binary, and a version that changes the grammar would otherwise fail silently:
  a fresh session opened while claiming to continue one, first seen an hour into somebody's
  run.

An unmetered kind is not skipped: it runs its turn and is asked everything above except the
readings, which is the whole of what can be checked for it. No shipped kind is in that state
today. `--live` needs a model, and spoolway names none for you: pass `--model`, or run it in
a project whose pipelines already name one for that kind. Pointed at a local endpoint, a live
check costs nothing.

### What the probes found

The row below was settled by installing the CLI, pointing it at a local OpenAI-compatible
endpoint and running turns against it — never by reading a `--help` string or a docs page. A
resume flag guessed wrong is the failure this guards against, and it is silent: a lane starts
a fresh session while claiming to continue one. Every clause of every other row was settled
the same way; `codex` is written out here because it is the row that took two passes to get
right, and both reversals are worth keeping.

**`codex` (codex-cli 0.147.0) — fully supported: launch, headless, resume, accounting.**

| Clause | What the binary did |
|---|---|
| Provider | Needs `wire_api = "responses"`; `"chat"` is refused outright by this version. llama.cpp serves `/v1/responses`, so a local endpoint works |
| Session dir | `<CODEX_HOME>/sessions/<yyyy>/<mm>/<dd>/rollout-<timestamp>-<id>.jsonl` |
| Usage record | An `event_msg` of type `token_count`, one per model **request** — a task that calls a tool has two — carrying `last_token_usage` and `total_token_usage`, each with `input_tokens`, `cached_input_tokens`, `cache_write_input_tokens`, `output_tokens`, `reasoning_output_tokens`. The `last` figures sum to the running `total` exactly, which is what makes summing them right and needs no dedupe. It is written when the request *finishes*, which is why a command run from inside a turn reads a rollout with no usage in it yet |
| Session id | **codex mints it and refuses any other.** `codex exec resume <a fresh uuid>` fails with "no rollout found for thread id", and `-c session_id=…` is rejected under `--strict-config` as an unknown field |
| `CODEX_HOME` | Relocates the whole state tree. A turn run with it pointed at a per-session directory wrote its rollout there and left **nothing** under `~/.codex` |
| Resume | `codex exec resume --last` under a per-session home recalled the first turn's answer and appended to the *same* rollout, which stayed the only one in the tree |
| Prompt | `-c model_instructions_file=<path>`. It *replaces* codex's base instructions rather than appending — the same prompt cost 7,503 input tokens without it and 3,055 with it — and a turn run that way still used its shell tool and wrote the file it was asked to |
| Approval | `--ask-for-approval` with `untrusted`, `on-request`, `never`. It is a *global* flag, and codex accepts globals before the subcommand, so `codex --ask-for-approval never … exec "<prompt>"` parses — one rendered args row serves both backends |
| Trust | The interactive frontend refuses to start in a directory that is not trusted, asking *Do you trust the contents of this directory?* and waiting for a keypress. The answer is a `[projects."<dir>"]` table with `trust_level = "trusted"` in `config.toml`, read from disk only — the same table passed as `-c projects."<dir>".trust_level=trusted` is parsed and ignored. Only the working directory is asked about; `--add-dir` paths are not. `codex exec` never asks (0.153.2) |
| Updates | The interactive frontend checks for a newer release on startup and holds the screen on `✨ Update available!` until somebody answers — option 1 being `npm install -g @openai/codex`, which swaps the binary other lanes are running. `-c check_for_update_on_startup=false` suppresses it: accepted under `--strict-config`, refused as `check_for_update_on_startupp`, and confirmed off against a home whose `version.json` named a newer release (0.153.4) |
| Git | Refuses to start outside a git repo: "Not inside a trusted directory and --skip-git-repo-check was not specified". A lane's worktree is a repo, so this only ever bites a scratch directory — which is why `verify --live` makes one |
| Effort | No flag; a config override, `-c model_reasoning_effort=<level>`. Accepted under `--strict-config`, and it reaches the turn — it lands in the rollout's `turn_context` as `effort: "high"`. `minimal`, `low` and `medium` arrive verbatim too, and so does a nonsense level: codex validates nothing here and hands the value to the provider |

An earlier pass left `headless` and `accounting` blank here, reasoning that codex mints its
own session id and so spoolway had nothing to look a transcript up by. The first half is
true; the conclusion was not. A session can be pinned by the directory it writes into as
well as by the name of the file — see [Two ways to pin a session](#two-ways-to-pin-a-session)
— and `CODEX_HOME` is what moves the directory. Both rows then rest on runs, not on
reasoning: the relocation, the resume, and every transcript reading were exercised end to end
and are re-checkable with one command.

A second pass overturned the last thing on this row that was written down as a gap. It said
codex exports nothing like `$CLAUDE_CODE_SESSION_ID`, so planning done in your own codex
session went unaccounted and nothing on spoolway's side could change it. That was a search
for the wrong name rather than a finding: **codex exports `$CODEX_THREAD_ID`** into every
command the session runs, and its value is exactly the id in the rollout's filename and its
`session_meta`.

| Clause | What the binary did |
|---|---|
| Interactive id | `env` run from inside a session printed `CODEX_THREAD_ID=019ffa86-b076-78a0-9ee3-cda940041941`, and the session's rollout is `rollout-2026-08-13T11-49-18-019ffa86-b076-78a0-9ee3-cda940041941.jsonl` |
| Both frontends | The same reading came back with `originator: codex-tui` and with `codex_exec`, so the interactive binary and the headless one agree |
| Nesting | A codex session started from inside a claude one carries *both* variables: codex inherits `$CLAUDE_CODE_SESSION_ID` and exports its own over the top. So the ambient lookup returns every session it finds rather than the first, and both are enrolled |
| When the usage lands | codex writes a turn's `token_count` when the turn **ends**. A command run from inside the turn reads a rollout with no usage in it at all — which is why a session with nothing readable yet is still enrolled, with a zero line, rather than skipped |

Run end to end afterwards, in a scratch project: a first `codex exec` turn enrolled the
session, `codex exec resume --last` banked 2 turns of it against the model `turn_context`
names, and a `spoolway spend` run from outside the session swept the rest.

A third pass settled codex's quota row the same way, and it needed something the other two did
not: a real ChatGPT sign-in. Every earlier reading on this machine came from a local
OpenAI-compatible endpoint, and that account has no plan window at all — `rate_limits` is
present on the event, but `primary` and `secondary` are both null. A bare API key gives the
same nulls. The row could only be written once a turn ran under an actual subscription.

| Clause | What the binary did |
|---|---|
| Where the figure is | An `event_msg` of type `token_count` carries a `rate_limits` object beside its `info`, in the same rollout the token usage is already read from. There is no cache file anywhere — the figure only exists per session |
| The authenticated reading | `~/.codex/sessions/2026/09/05/rollout-2026-09-05T09-51-18-01a0708c-ec8f-7a01-b4f9-99f6337e1a05.jsonl`, `auth_mode: chatgpt`, last `token_count` event: `"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1788611977},"secondary":{"used_percent":2.0,"window_minutes":10080,"resets_at":1789151593},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}` |
| Shapes | `used_percent` is a float, not claude's integer. `resets_at` is a Unix timestamp in seconds, not claude's RFC3339 string. `window_minutes` is 300 and 10080 — five hours and seven days, the same two windows claude declares |
| The unauthenticated reading | The same object with both windows null and the account fields beside them null: `"primary":null,"secondary":null,"credits":null,"plan_type":null`. `limit_id` stays `"codex"`. Seen on `~/.codex/sessions/2026/08/13/rollout-2026-08-13T12-06-16-019ffa96-38fe-7ec1-b8d2-d9b8c4698e45.jsonl`, a turn against the local endpoint |
| Freshness | Nothing in the object says when it was fetched. The `token_count` event's own `timestamp` dates the reading instead, which is what the five-hour staleness rule is applied to |

The null case is why the probe has a fifth outcome the claude reader never needed. A rollout
whose `rate_limits` has both windows null parsed correctly and is not a broken file, so calling
it unparseable would be wrong. It reports no reading, which the dispatcher treats the same as
every other miss: it blocks nothing. A rollout with only one window null is a shape nobody has
seen, and that one is reported as malformed.

`usage_limit` stayed `None` on this row. No real run has left a limit phrase in a codex pane to
quote, and this pass did not invent one.

**codex declares no cache lifetime, and cannot.** Its `token_count` reports how many input
tokens were served from cache, so cached input is priced correctly, but it carries no
lifetime anywhere — re-checked against a real rollout by scanning every record for one under
any name. OpenAI's prompt caching is automatic and exposes no TTL a client can read. That no
longer matters for session reuse: the horizon a carried session is weighed against is the
store's own age, read off its file's mtime rather than anything the record declares — see
[Cache warmth is a model's fact](#cache-warmth-is-a-models-fact).

**Exercising `--live` for free.** Two kinds run their live half at no cost, and both pass every
reading:

```
spoolway agent verify pi          --live --model <a model your server serves>
spoolway agent verify codex       --live --model <a model your server serves>
```

codex needs `wire_api = "responses"` in its config to talk to llama.cpp; see the provider row
above. `claude` remains the only kind whose `--live` half spends. The e2e harness's `live`
tier is exactly the codex command, opted in per kind by naming the model — see
`docs/testing.md` — so the row above stays re-checked against whatever version of the binary
is actually installed, rather than only against the one it was settled on.

### Local models, and the pi integration

The local half of the pipeline runs on `pi`, which is what makes running against your own
model server the normal case rather than a workaround. Three things fall out of it:

**A session id you chose.** pi accepts a session id, and spoolway mints one per lane and
passes it. That is what makes finding the lane's transcript afterwards a lookup by a name
spoolway chose, rather than a guess from a working directory and a timestamp — which is what
[cost accounting](cost.md) is built on.

**Self-priced transcripts.** pi prices its own transcripts and reports zero for a local
model, which is the whole thesis of the design stated as a measurement rather than a claim.

The shipped local profile passes `--no-approve`, which skips the project-trust dialog a lane
would otherwise hang on with no error anywhere. It loads project skills from `.pi/skills`,
the same as every other kind that has a directory of its own — see [Skills](#skills) below.

### Claude Code lanes

The shipped review profile runs `claude`. Two details are worth knowing:

- The prompt is passed as a **file**, not as literal text, because handing the text flag a
  path would append the path string and quietly drop the prompt.
- The lane is given the project's `.spoolway/` directory and the project's own home
  directory as extra workspace directories. A lane's own source is in its worktree, but the
  task file it works from is not — and every permission mode prompts on a read outside the
  workspace, so without both of these the first thing a lane would do is stop and ask a
  person who is not there.

`bypassPermissions` is deliberately *not* the default mode. Plenty of organisations disable
it outright, and a pipeline that only runs where it is allowed is not one to build on.

### Leaving a pane without closing it

A row's `quit` is the line typed at that kind's own prompt, then submitted, that ends its
session while leaving the pane it ran in standing at a shell. It is read by
[`Mux::vacate_lane`](dispatcher.md#vacating-a-pane), which is how a task's pane carries from
one step to the next instead of being closed and split again — see that section.

Only `claude` carries one today: `/exit`, established by probe against the real binary rather
than read off documentation. Submitted at a settled prompt, it takes the agent out of `herdr
agent list` in well under a second and leaves the pane standing at its shell, with its own
working directory intact. Two Ctrl+C in quick succession do not end Claude Code, and neither
does Ctrl+D at an empty prompt — the reason a kind's gesture is a line typed at the prompt
rather than a keystroke.

Every other kind's row is `None`. A gesture guessed at the wrong kind either does nothing or
lands as a stray keystroke in somebody's conversation, so a row is filled in only against a
session actually driven and watched leave — the same discipline `headless` is held to above.
A kind whose `quit` is `None` has its panes closed and re-split, one per step, which is what
every kind's panes got before `claude` had a row.

## The argument template

The argv a lane is started with is fixed per `kind` in `agent::ADAPTERS`, not written into the
config — pointing a step at a different CLI is still a config edit, but it is `kind:` that does
it, not hand-spelled flags. Each row's template substitutes:

| Placeholder | Resolves to |
|---|---|
| `{model}` | The resolved model for this step |
| `{prompt_file}` | Absolute path to the step's prompt |
| `{task_file}` | Absolute path to the canonical task file |
| `{worktree}` | Absolute path to the task's worktree |
| `{repo}` | Absolute path to the project root |
| `{state_dir}` | Absolute path to the project's `.spoolway/` — the prompts a lane reads from outside its worktree |
| `{project_home}` | Absolute path to the project's own home — the task file a lane reads from outside its worktree |
| `{git_dir}` | Absolute path to the repo's shared `.git` — the one write grant of the three `--add-dir` paths; the objects and branch ref a lane's `git add`/`git commit` write from inside its worktree live there, outside `{worktree}` and otherwise read-only to it |
| `{session_id}` | The session id spoolway minted for this lane |

Drop `{session_id}` and the lane still runs. For a kind that takes an id, it then spends
unaccounted — the lookup has nothing to match. `codex` carries no `{session_id}` at all,
because it will not take one: its sessions are pinned by `$CODEX_HOME` instead.

**Some of it is not argv.** The per-session home a directory-pinned kind gets is an
environment variable, computed rather than templated: it is spoolway's own state directory
and this session's id, neither of which a row could spell — see
[`crate::agent::Adapter::session_env`].

A kind whose `agent::ADAPTERS` row has no `args` template at all cannot be launched: a
profile naming one is refused by `spoolway doctor` and at lane start, rather than run with no
flags. This is the one clause of a row that is a refusal; a row missing only its
*accounting* half would launch and run, unmetered, and be told what that costs — no shipped
kind is in that state. See [Accounting is
optional](#accounting-is-optional-and-its-absence-is-a-cost-not-a-refusal) above.

No part of the argv is a project's to choose. Every flag in it is spoolway addressing a CLI
it knows, with paths and ids only it can compute, and a project that needs a lane launched
differently is describing a new kind — a row in the adapter table — rather than a config key.

## Which model a step actually runs

Exactly what its `model:` names — nothing else. A step with no `model:`, or a blank one, is
refused by `pipeline check` and `doctor`, and a lane refuses to start rather than launch an
agent with an empty model flag. See [Pipelines](pipelines.md) for the step key itself.

Nothing here reads how many times the task has been through this step: a step's model is a
property of the pipeline, so reading the file tells you what will run.

### Effort

A step's `effort:` is a free string, handed straight through to whatever argv its agent kind
carries an effort on — a flag on one kind, a config override on another:

```yaml
- id: review
  agent: claude
  prompt: reviewer
  model: claude-opus-5
  effort: high
```

There is no level list anywhere in spoolway. Which levels a model accepts is the model's own
fact, changes when the model changes, and a copy of that list in this binary would only go
stale silently — claude already warns and falls back when a level is wrong, and codex does
not validate the value at all, handing whatever it is given to the provider. `pipeline check`
knows one thing about a level: whether the step's agent kind can carry one at all (see the
table above), and refuses `effort:` on a kind that cannot, or the literal `auto` — a word
spoolway used to resolve for itself, against sensitive paths nothing computes any more, so
nothing resolves it now.

Absent `effort:` sends nothing, and the model runs at its own default.

The two kinds that carry one spell it differently, which is why the row is a template rather
than a flag name:

```
claude    --effort high
codex     -c model_reasoning_effort=high
```

`pi` carries none: its `--thinking` takes a token budget, which is a different axis from a
named level, so it is not wired to this key.

Retired: `[effort]` in config.toml, which held a tier → model table (`effort: high` meant
"swap in `tier_models.high`'s model", not an effort at all) and the `sensitive_paths` that
decided what `effort: auto` meant. Both jobs are the step's now: name the model directly, and
name the effort separately. An old `[effort]` table still parses and is dropped on the next
save.

### Skills

A kind that has a notion of project skills at all names its own directory for them: `claude`
reads `.claude/skills`, `codex` reads `.agents/skills`, and `pi` reads `.pi/skills`. That is the
same directory `spoolway install` writes a project's skills into, so the two cannot name it
differently.

**Whether the kind loads skills at all is the whole of what `pipeline check` reads.** A
`skills:` on a step that starts no lane — a command step or a terminal one — is refused, and
so is a `skills:` on an agent kind whose adapter row says it loads none, which would launch
and quietly ignore the invocation. All three shipped kinds load skills, so nothing today
trips the second refusal; the row is a yes-or-no rather than a directory precisely so a kind
that does not can be added without teaching the check anything.

The names themselves are never looked up on disk. spoolway can see a project's own skills
directory and the user's, but a plugin installs its skills somewhere spoolway has no way to
enumerate, so "in neither directory" does not mean "not installed" and refusing on it would
fail every pipeline naming a plugin skill. The agent resolves the name at launch, which is
the only place it can be resolved.

## Concurrency and the model server

`concurrency` caps a **harness**: how many copies of one binary spoolway runs at once. That is
a real question for a hosted agent, where the answer is a fact about an account and its rate
limits, and no shipped profile answers it — a fresh project does not know the account or model capacity it would need to assert one, so it states it explicitly on the profile when it wants a cap.

It is the wrong question for a local one, and the local profiles ship without it. `pi` and
`codex` are both harnesses in front of the same server, and what that server
serves at once is a property of the weights it is holding: swap a 3-slot model for a 1-slot
one and the real limit changes while a number on the profile does not. That count belongs to
the model — [`models."<glob>".slots`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is) —
and it replaces `concurrency` for any step naming a model that sets it.

**A step capped by neither is capped by nothing.** `slots` falls back to `concurrency` and a
local profile sets none, so a project that has not yet written its models into `[models]` runs
as many local lanes as the queue offers. `0` has always meant unlimited and still does; what
changed is that it is now reachable by leaving things alone rather than by asking for it, so
`spoolway doctor` names each step in that state. Write the model's `slots` and the note goes
away.

A model's context window is not a profile setting either — see
[`[models."<glob>"]`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is)
in the configuration reference. Two profile settings read it at run time: `session_reuse_ctx`
and `session_blocked_ctx` each take their percentage of it. A `session: true` step against a
model with `context_window` unset reuses an earlier session whenever its reuse ceiling is off
(`session_reuse_ctx = 0`); it refuses reuse on size only when a nonzero ceiling is set and so
needs a window to measure against.
The number does not truncate, chunk or cap anything a lane writes.

## What ends a lane

No profile carries a clock any more. A busy lane is never escalated, whatever it is doing or
how long it has been doing it — the dispatcher decides nothing about one until its turn ends.
That is not a retry budget, and it is not a verdict on the work: a lane that writes steadily and
never finishes simply holds its worker slot until somebody looks at the board.

What still ends a lane is a settled one that never reported. A turn appends to its transcript as
it goes — the message, each tool call, each result — so a lane that ended its turn without
`spoolway report` is one whose transcript has stopped growing. It is sent the report contract
again, up to three times, each time it has written something since the last reminder; a lane
that goes quiet right after a reminder is blocked on the very next pass, with nothing further to
wait out. There is no exception for a gated step — a gated lane reports like any other, and the
waiting happens after it, on `paused`.

Read off the lane's *transcript*, not its pane. A lane wedged inside one long tool call is a file
that has stopped growing; reading the screen instead answered a different question, since an
agent renders a spinner and a token meter while it waits, so a wedged lane looked busy for as
long as it was drawing.

## What confines a profile

Nothing does, and that is worth stating rather than leaving to be discovered.

There used to be two layers: a kernel one, Landlock via a shim on every local lane, and the
harness's own sandbox behind `sandbox.mode` and `sandbox.domains`. Both are gone. The kernel
layer was real confinement and it worked, but it bought that at the cost of a `.git` carve-out
with a page of consequences of its own, a shim directory on every lane's `PATH`, and a config
surface — `sandbox.read`, `sandbox.write`, `sandbox.deny` — that a project had to keep in step
with its own toolchain before a build would run in a lane at all.

So a lane now runs with the privileges of whoever started the dispatcher, and the honest way
to read that is: **a lane can do what you can do.** Run one on a machine where that is
acceptable. Nothing inside spoolway bounds it further — see [Reach](concepts.md#reach) — so
whatever confinement a project wants is the agent's own harness settings, outside this
repository entirely.
