---
domain: jobs
covers:
  - "src/jobs.rs"
  - "src/cron.rs"
---

# Jobs

A job runs a routine on a schedule. It is a five-field cron expression, a pipeline, and a
routine target that a person already saved under `.spoolway/routines/`.

## Overview

spoolway has no clock of its own. A routine reaches the queue only when a person opens the
queue screen and presses a key, and the dispatcher exits as soon as the queue empties. For
most of the day no spoolway process is alive to start anything.

A job closes that gap. The dispatcher's own pass fires a job when the local clock crosses its
expression. The routine's documents are queued through the same path the queue screen's `r`
pane runs — a folder as one batch, a single document alone. To stay alive for a window that
falls when the queue is empty, `spoolway dispatch` now stays resident on an empty queue for
as long as any job is enabled.

A job only ever runs work that already exists as a routine. Nothing here creates a routine,
schedules an arbitrary command, or writes to a file under `.spoolway/routines/`. The routine
documents are read and never changed.

Nothing writes a job yet either. The two stores hold jobs and every command reads them; the
screen that writes one is a later task. Until then you edit the store files by hand. A store
with no job in it lists nothing, and the dispatcher behaves exactly as it does today.

## How it works

At the top of every dispatcher pass, before it reads the queue, the pass fires any due job.
A `--dry-run` pass makes no changes and so fires nothing.

A job is due when it is enabled and its expression matches the current local minute. Its
routine is then queued: a folder target as one batch with fresh ids and `depends_on`
remapped onto them, a single `.md` target alone with its `depends_on` emptied. Every queued
document is put on the job's own pipeline. Because this happens before the queue is read,
the freshly queued documents are dispatched by the same pass.

The fired minute is written to `jobs.state.json` in the project's machine home. The default
pass interval is ten seconds, so six or more passes cross the same 03:00 minute. The state
file is what makes the job fire once: a minute already recorded there is not fired again.

A missed window stays missed. A job is only ever asked whether it matches the *current*
minute, so a window that passed while no dispatcher was running is never queued later.

A job whose previous run is still in the queue skips its next window. The skip is recorded
in the state file and reported once as a pass action naming the job. A nightly audit that
takes thirty hours never has two copies in flight.

A firing is reported as a pass action naming the job, the same way a skip is. Trouble with
one job goes to the pass's problem list and never fails the pass — the other jobs still run.

### The resident dispatcher

While any job is enabled, `spoolway dispatch` no longer stops on an empty queue. It stays
resident so it is alive when the next window comes round. This holds on a cold start with
nothing queued, where the run used to exit 3, and on the pass that drains the last task.

The plain run prints, once per spell of empty queue:

```
queue is empty. 2 jobs enabled — staying up for them.
next: nightly-audit, Mon 8 Sep 03:00  (in 5h 48m)
ctrl-c stops.
```

The board shows the same three facts in its own dim style, in place of `nothing queued`. If
no enabled job will ever fire, the `next:` line reads `no job will fire — run spoolway
doctor` instead.

With no job enabled, nothing changes. An empty queue still stops the run, a cold start with
nothing queued still exits 3, `ctrl-c` still stops a resident run, and
`dispatch.tear_lanes_on_stop` still applies when it does. See
[When it stops](dispatcher.md#when-it-stops).

## Key concepts

| Term | What it means here |
| --- | --- |
| Job | A named cron expression, a pipeline, and a routine target, in one store. |
| Store | A TOML file of `[jobs.<name>]` tables. There are two, merged into one list. |
| Scope | Which store a job came from: `user` or `project`. |
| Routine target | A path under `.spoolway/routines/`: a folder, or a single `.md` file. |
| Firing history | `jobs.state.json`: the last minute each job fired, and the ids it queued. |
| Due | Enabled, and the expression matches the current local minute. |

## The cron grammar

Five fields, in order: minute (`0-59`), hour (`0-23`), day of month (`1-31`), month (`1-12`
or `jan`-`dec`), day of week (`0-6` or `sun`-`sat`, where `0` is Sunday).

Each field is a comma list of terms. A term is one of `*`, `n`, `a-b`, `*/n` or `a-b/n`.

`@hourly`, `@daily`, `@weekly` and `@monthly` are whole-expression aliases.

A value out of range, or a sixth field, is refused with the field named.

When *both* day fields are restricted, a match on either one fires the job. A field counts as
restricted when it does not begin with `*`, so `*/2` is not restricted but `1-5` is. This
means `0 3 13 * fri` fires on the thirteenth and on every Friday, not only on a Friday the
thirteenth. This follows Vixie cron.

Times are the dispatcher machine's own local time. A laptop that travels fires at 03:00
wherever it is.

## Where a job lives

| Path | Scope | Tracked |
| --- | --- | --- |
| `~/.spoolway/<project>/jobs.toml` | `user` — the default | No |
| `<checkout>/.spoolway/jobs.toml` | `project` — shared with the team | Yes |
| `~/.spoolway/<project>/jobs.state.json` | firing history, whichever store the job is in | No |

Both stores are merged into one list, the user store first. A missing file is an empty list,
not an error. A file that is present but will not parse is an error. One name defined in both
stores is refused, and both paths are printed.

A store file holds `[jobs.<name>]` tables and nothing else at the top level:

```toml
[jobs.nightly-audit]
schedule = "0 3 * * 1-5"
pipeline = "impl_fast"
routine  = "nightly"
enabled  = true          # optional; absent means enabled
```

`routine` is a path relative to `.spoolway/routines/`. An absolute path or a `..` component
is refused before any command touches it.

## Usage

```
spoolway jobs list                 # every job across both stores
spoolway jobs list --json          # the same rows, machine-readable
spoolway jobs run <name>           # fire one job now, ignoring its schedule
```

`spoolway jobs list` prints a row per job — `NAME`, `SCOPE`, `SCHEDULE`, `PIPELINE`, `NEXT`,
`LAST` — then a count line and a per-store count:

```
NAME           SCOPE    SCHEDULE      PIPELINE    NEXT        LAST
nightly-audit  user     0 3 * * 1-5   impl_fast   in 5h 48m   ok, 19h ago
weekly-deps    project  0 3 * * sun   impl        Sun 03:00   ok, 3d ago
lint-sweep     user     */30 * * * *  impl_fast   paused      -

3 jobs, 2 enabled.
  ~/.spoolway/spoolway/jobs.toml    2
  .spoolway/jobs.toml               1
```

`NEXT` reads `paused` for a disabled job, `bad expr` for one whose expression will not parse,
`never` for one that parses but never comes round, and otherwise a relative time within a
day, a weekday and time within a week, or a full date beyond that. `LAST` is `-` until the
job has fired, then `ok, <time> ago`.

`spoolway jobs run <name>` queues the routine the same way a scheduled firing does and
records the run, so `LAST` updates and the overlap guard sees it. The job's next scheduled
firing is unaffected. The command is refused from inside a lane.

A `--json` row carries `name`, `scope`, `schedule`, `pipeline`, `routine`, `enabled`,
`source`, `next` (local RFC 3339, or `null`), and `last_fired` (epoch seconds, or `null`).
The `schedule_error` key appears only on a row whose expression will not parse, so a script
can test for its presence.

## Diagnostics

`spoolway doctor` checks every job across both stores:

| Check | Fails when |
| --- | --- |
| job `<name>` schedule fires | The expression will not parse, or parses but never comes round (`0 0 30 2 *`). |
| job `<name>` routine exists | The target path is gone, or `routine` escapes `.spoolway/routines/`. |
| job `<name>` pipeline is defined | `pipeline` names no defined pipeline. |
| job stores load | The two stores together will not load — a name in both, or malformed TOML. |

## Reference

| Path | Responsibility |
| --- | --- |
| `src/jobs.rs` | The stores, the merge, `fire_due` at the top of a pass, the firing history, and the "staying up" lines. |
| `src/cron.rs` | The five-field expression: parsing, matching a minute, and the next firing after a given instant. |
| `src/commands/jobs.rs` | `spoolway jobs list` and `jobs run`. |
| `src/commands/queue.rs` | `queue_routine_target`, the queueing path a job shares with the `r` pane. |
| `src/commands/dispatch.rs` | The resident dispatcher and `EXIT_EMPTY_QUEUE`. |
| `src/commands/doctor.rs` | The job checks above. |

## Constraints

A supervisor loop built on `spoolway dispatch` exiting 3 will hold a process open forever
once a job exists. Exit 3 now fires only when no job is enabled.

Nothing tells you a window was skipped except the pass action, which scrolls past, and the
`LAST` column going stale. Nobody is reading either at 03:00.

Daylight-saving transitions are handled. A local minute that a spring-forward jump skips is
resolved to the next real matching instant. A local minute that a fall-back repeats fires at
whichever of its two occurrences is still ahead.

There is no `[jobs]` section in `config.toml`, no `confdoc` row, and no config key of any
kind. A job is defined only in a store file.

## Related

[Planning](planning.md#routines) covers routines themselves — how a folder and its documents
are saved, and how ids are minted when one is queued. [The dispatcher](dispatcher.md) covers
the pass that fires a job and the loop that stays resident for one. [CLI
reference](cli-reference.md#spoolway-jobs) has the full command surface.
