---
name: merge-pull-requests
description: Merge every open pull request on this repository into main — order the stacks, resolve the conflicts directly, fix whatever is red or hung on the branch it belongs to, verify the merged tree, push, prune the branches, and reinstall the binary. Triggered by a human after a dispatch run has left a batch of task pull requests open.
disable-model-invocation: true
---

# merge-pull-requests

A dispatch run leaves one pull request per task, and they pile up. This skill empties
the pile in a single pass: every open pull request ends up on `main`, and the binary on
`PATH` ends up being the thing that was just merged.

**Resolve, do not ask.** A conflict here is almost never a product decision. Two tasks
touched the same table, the same doc comment, the same sample frame. Work out what both
changes wanted and write the line that gives them both it. Stop and ask the human only if
the resolution would drop behaviour one of the tasks was accepted for.

**Fix, do not defer.** The same goes for everything else the pass turns up: a red check, a
hung job, a branch that no longer builds against `main`. The pile is only empty when the pull
requests are merged, so a problem found on the way is this pass's problem. Diagnose it, fix it
on the branch it belongs to, and carry on. A pull request handed back untouched with a note
about what is wrong with it is the one outcome this skill exists to avoid.

**Start nothing on your own initiative.** This pass reads the queue and it stops there. It
never runs `spoolway dispatch`, never resumes a task, and never starts a lane *because it
decided to*. A dispatcher this pass starts unbidden is one nobody asked for and nobody is
watching: it cuts worktrees, opens panes and puts real agents to work on a tree that is
halfway through a merge. If a dispatcher is already running, step 1 has already told you —
wait for it rather than reaching for a restart.

**The human can ask, and then you do it.** Restarting is safe by design, not by luck —
`Dispatcher::sweep_on_stop` (`src/teardown.rs`) settles the books "without touching any of
it": no worktree, no workspace, no pane, no tab is removed, and the lane, agent and any
`background: true` command alike, is left running. A command step is spawned under
`libc::setsid()` with null stdio precisely so it outlives the pass that started it, its
timeout clock is the `.pid` file's mtime so a restart does not reset it, and the launch
counter is forgiven so the next run does not read the surviving lane as a failed launch.
What is *not* free is the timing and the keystroke — see *Restarting the dispatcher* below.

The first of those two is worth more than it looks, because the way it gets broken is not by
deciding to break it. See the heredoc entry under *What has bitten before*: a command can be run by
writing about it.

**Merging is local, not on the forge.** Nothing is merged through `gh pr merge`. You merge
into `main` on this machine, verify the result compiles and passes, and push. GitHub then
marks the pull requests merged on its own, because their head commits are on `main`.

## Procedure

### 1. Survey

```
gh pr list --state open \
  --json number,title,headRefName,baseRefName,mergeStateStatus,statusCheckRollup
spoolway queue list
git fetch origin --prune
```

`mergeStateStatus` is `CLEAN` for a pull request that would merge untouched and `DIRTY` for
one that conflicts. Read `baseRefName` on every row: a base that is not `main` is a
**stacked** pull request, sitting on another branch in the same list.

Expect `BLOCKED` on nearly every row, and do not read it as a problem. It is the forge
refusing its own merge button, usually for a review this repository never collects. You merge
locally, so it says nothing about whether the branch merges.

`statusCheckRollup` is the state of continuous integration for each head commit. Take it in
at the survey, but do not act on it yet — step 3 decides which of these rows actually have to
be green.

`spoolway queue list` says whether a dispatcher is running. That matters twice — for what is
still coming (step 8) and for the install (step 9).

### 2. Update main, then order the merges

```
git checkout main
git merge --ff-only origin/main
```

Local `main` is usually behind. Do this before anything else, or you will resolve conflicts
against a tree nobody has.

Then order what is left. A stack merges **once, at its tip**: the tip branch already contains
every commit below it, so merging the tip carries the whole stack in and closes every pull
request in it. Merging the bottom separately only creates a second conflict to resolve.

Everything not in a stack merges in any order, but merge them one at a time — a conflict you
can attribute to one branch is a conflict you can reason about.

### 3. Wait for the checks

Every pull request on this repository runs the `ci` workflow, whatever its base. A merge
before that finishes is a merge with no evidence behind it. Wait for it.

Wait on **the branches you are actually going to merge** — the tips from step 2, and nothing
else:

```
gh pr checks <number> --watch
```

`--watch` holds until every check on that head commit reaches a conclusion, then prints the
table and exits non-zero if any of them failed. Run it per tip. There is no hurry to be clever
here: `verify / test` takes something like eight minutes, and a merge pushed on top of a red
branch costs far more than that to unpick.

The checks you will see are `changes`, `verify / test` and `verify / audit`. `dress-rehearsal`
reports `SKIPPED` on pull requests by its own `if:`, and a skipped or neutral check is not a
failure.

**Only the tip's checks decide a stack.** The tip is the commit that lands on `main`, so the
tip is the only place the checks describe the merged tree. A branch below it may be legitimately
red: it can carry half a change, with the other half — the part that makes the suite pass —
sitting in the branch above. Judge that red by reading it, not by assuming either way.

**A red tip is something to fix, not something to defer.** It stops the merge, but stopping
the merge is not the same as leaving the pull request for somebody else. Open the failing job,
find what is actually wrong, and fix it on the branch: commit the fix to the pull request's own
head, push, wait for the checks again, and merge it with the rest of the pile. These are this
repository's own task branches, not contributions from strangers, and a red tip left for "the
next pass" is a pull request nobody comes back to.

The judgement is the same one conflicts get. Fix it yourself unless the fix would drop
behaviour the task was accepted for, or unless the red is the task's *premise* being wrong
rather than a mistake in carrying it out — a suite that fails because the feature does not work
is not a suite to adjust. Those two stop and ask; everything else you fix.

Below a tip, open the failing job, find the commit further up the stack that resolves it, and
say so in the report. If nothing up the stack resolves it, treat it as a red tip and fix it the
same way.

**A check that hangs is a red check wearing a disguise**, and the expensive mistake is reading
it as a slow one. Compare the step against its own duration on a green run before concluding
anything: a job sitting at five times its usual time is stuck, and waiting out a 45-minute job
timeout to be told so costs more than the whole merge. Do not wait for the forge — it serves no
log until the job is over. Reproduce it locally instead: `git worktree add` on the branch,
build, run the one suite under `timeout`, and read the hung process (`ps`, `/proc/<pid>/wchan`)
while it is still hanging. The last `ok` the suite printed names the line that did not return.

A branch whose checks never started is usually one pushed before the workflow existed. Push an
empty commit or re-run the workflow and wait; do not merge it unchecked.

### 4. Merge

```
git merge --no-ff origin/<branch> -m "Merge PRs #158, #161: cursor-in-gap + board-key-map"
```

That message shape is this repository's convention for a batch: the numbers, then the task ids
behind them. One branch alone gets `Merge PR #161: board-key-map`.

### 5. Resolve

```
grep -rn '^<<<<<<<\|^=======$\|^>>>>>>>' .
```

Three kinds of conflict show up here, and each has a different answer.

**Rust that both sides restructured.** Take the newer structure from `main` and re-apply the
branch's change inside it. A branch written against last week's `src/status.rs` will reference
bindings that no longer exist — `git show main:<file>` and `git show origin/<branch>:<file>`
side by side is how you find out which name survived. Never keep a hunk that mentions a
binding `grep` cannot find a definition for; it will compile only if you were lucky.

**Doc comments.** Both sides usually rewrote the same paragraph for different reasons. Keep
both facts. Prefer `main`'s wording where it describes structure that still exists, and the
branch's wording where it describes what the branch changed.

**ASCII frames in `docs/`.** The sample boards in `docs/dispatcher.md` are the ones that
conflict, and they are the ones people get wrong by hand. Transform `main`'s frame
mechanically rather than retyping it — a short Python script that strips or inserts a fixed
number of columns per line keeps every figure column aligned. Then read the result back and
check that a column heading still sits over its values.

### 6. Verify

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

All three, every time, before the push. The forge checked each branch on its own; nothing
checked them combined, and the merge you just resolved by hand exists only here. This is the
only verdict there will ever be on that tree.

Then look at the thing. A merge that compiles can still render a broken table, and no test
pins every frame:

```
./target/debug/spoolway queue list
```

Run the **debug** binary, not `spoolway`. The one on `PATH` is the last build somebody
installed, so it will happily show you the old layout and tell you nothing.

### 7. Push, then prove `main` is green

```
git push origin main
gh pr list --state open
```

The second line is the check, not a formality. Every pull request whose head landed should now
be gone from the open list, the stacked one included. Anything still open did not actually
merge — find out why before moving on.

Then run `main`'s own gate against what you just pushed. **The push starts nothing.** `ci.yml`
triggers on `pull_request`, `schedule` and `workflow_dispatch` — there is no `push:` entry — so
a merge pushed to `main` is a merge no workflow has looked at, and nothing will look at it
until the 03:17 UTC schedule the next morning. Dispatch it yourself:

```
git rev-parse HEAD                                  # the sha the run has to be for
gh workflow run ci.yml --ref main                   # the tier input defaults to nightly
gh run list --workflow=ci.yml --branch main --event workflow_dispatch \
  --limit 5 --json databaseId,headSha,createdAt     # find the run for that sha
gh run watch <id> --exit-status
```

Match the run to the sha before watching it. `gh workflow run` prints no run id, and the newest
dispatch against `main` can easily be a stale one or somebody else's — watching the wrong run is
how a pass reports green for a tree nothing tested.

**The pull requests' own checks do not answer this.** They ran the `pr` tier, and
`dress-rehearsal` excludes itself from pull requests by its own `if:`. `main` gets the `nightly`
tier and the rehearsal on top of it, so main's gate is a strictly larger suite than anything
that ran on the branches — and step 6's local `cargo test` is smaller still. A column of green
pull requests is no evidence at all about the run you have just dispatched.

**A red run here is this pass's, on the same terms as a red tip.** Open the failing job, find
what is actually wrong, fix it on `main`, push, and dispatch again until it is green. What not
to do is stop at the push and call the pile empty: the merges are the reason anybody is looking
at `main` today, and a red left here sits until the next morning's schedule, where it surfaces
as a mysterious nightly failure rather than as the thing this pass walked past.

A failure need not be the merges' doing to be this pass's to fix. `dress-rehearsal` tests a
*release*, not a branch, and it goes red for things no pull request ever touched — see the
fixture entry under *What has bitten before*.

### 8. Anything still in flight

If step 1 found a dispatcher running, it is going to produce more pull requests. Wait for it
rather than declaring the pile empty:

```
spoolway queue list      # until the dispatcher is no longer running
```

A task reaches a pull request at its `handover` step, so a queue that still shows work has
work still to merge. When the new pull requests appear, go back to step 1 and run the whole
procedure again on them. **Do not queue new tasks** to fix anything you found on the way — a
fix this pass needs, you make by hand, on the branch that needs it. Queueing is for work that
is somebody's next decision, and it would hand this pass a moving pile it can never finish.

### 9. Install the build

Only once `main` is green (step 7), the queue is empty and no dispatcher is running:

```
cargo build --release
spoolway queue list                       # confirm again — nothing in flight
ls -l $(command -v spoolway)              # read this before writing to it
cp target/release/spoolway ~/.cargo/bin/spoolway.new
mv -f ~/.cargo/bin/spoolway.new ~/.cargo/bin/spoolway
spoolway --version
```

**Never plain-`cp` over the binary.** The `cp` truncates and writes through the inode the
running process is executing. A half-written binary is a dispatcher that dies mid-pass, and
lanes that start after it pick up a build nobody meant to be running.

**Install by rename instead**, as above. `mv` replaces the directory entry in one step;
anything already executing keeps the old inode and never sees a partial file. That makes the
install safe even with a lane live, which the plain `cp` never is.

**Resolve the path first.** `~/.local/bin/spoolway` on this machine is a *symlink* into
`~/.cargo/bin/`, so `cp target/release/spoolway ~/.local/bin/` follows it and writes straight
through to the real file — the exact inode the running dispatcher is executing. Write to the
resolved path, not the link.

### 10. Take what the new binary writes

A merge is the thing that makes this project's own `.spoolway/` older than the binary reading
it. New settings and new pipeline keys land in `assets/`, and the installed copy under
`.spoolway/` keeps whatever the last `sync` left. So `doctor` starts reporting files behind
straight after the install, and the drift is the merge's, not the project's.

```
spoolway sync --dry-run                   # read this before taking it
spoolway sync
spoolway doctor
```

Read the dry run rather than skipping to `sync`. It rewrites spoolway's half of files a
person also writes in, and a pull request that changed a default is exactly the case where
you want to see which one before it lands. Leave `--force-contract` and `--replace` alone
here: both discard human edits, and neither is a merge's decision to make.

Commit the result if anything changed. It is a change to this repository's control plane and
belongs in the history with the merge that caused it.

### 11. Prune

Clear the branches the merge made dead:

```
git push origin --delete <branch> ...
git branch -d <branch> ...
```

Expect that `--delete` to fail with `remote ref does not exist`. GitHub deletes a head branch
itself when a pull request merges, so the branches are usually gone before you ask. That is
success, not a problem — `git fetch origin --prune` clears the tracking refs left behind.

The `worktree-cleanup` skill takes the rest — stale worktree registrations and merged local
branches across every project.

A dangling worktree in `git worktree list` is **usually not spoolway's**. Spoolway removes
every checkout it cuts, in `Dispatcher::tear_down_checkout`, and it cuts them under
`~/.spoolway/<project>/worktrees/` and nowhere else. A registration anywhere else came from
another tool: `~/.herdr/worktrees/` is herdr's, and `.claude/worktrees/` and anything under
`/tmp/claude-*/` are Claude Code's. Check the path before reporting spoolway left something
behind.

## Restarting the dispatcher

Only when the human asks for it. The mechanism is safe; the timing and the keystroke are
what go wrong.

**Do it between the merges and the install, not during them.** A dispatcher started while
the tree is halfway through a merge puts real agents to work on that tree. Finish the
merges, push, then stop it, install, and start it again.

**One `ctrl-c`. Exactly one.** The handler restores `SIG_DFL` on its first press — read it
in `src/platform.rs`, it says so:

    // Back to the default first, so a second press kills outright
    // rather than setting a flag that is already set.

So the first press asks for a graceful stop and the second *kills the process outright*. The
graceful stop is not instant — `sweep_on_stop` harvests a whole transcript per live lane
before it returns — so the second press is easy to talk yourself into. What it costs: the
lane's spend is never banked, the launch counter is never forgiven, and `Lock`'s `Drop`
never runs, so `dispatch.pid` is left behind naming a dead process. Press once and wait for
the shell prompt to come back.

A stale `dispatch.pid` is not fatal — `Lock::holder` checks `is_running` and treats a dead
holder as no holder — but nothing banked the spend of the lane that was live when you killed
it, and that is gone for good.

**Send the keys to the dispatcher's own pane, not a new one.** Find it by its foreground
process group rather than by guessing:

```
herdr pane list
herdr pane process-info --pane <id>        # foreground_processes names `spoolway dispatch`
herdr pane send-keys <id> c-c              # `ctrl-c` is not a spelling herdr accepts
herdr pane send-text <id> "spoolway dispatch"
herdr pane send-keys <id> enter
```

`c-c` is the spelling that works. Do not try several spellings to see which lands: the ones
that work all land, and that is how one `ctrl-c` becomes three.

**Expect two screens before the board, and answer both.** Since #240 and #246, `spoolway
dispatch` does not go straight to the board: it draws what is queued behind an `[enter] start
a dispatcher` gate, then holds the run's warnings behind `[enter] start the run`. A
dispatcher that looks like it "did not start" is usually just sitting on one of those. Verify
it actually took the lock rather than trusting the screen:

```
spoolway queue list                        # `dispatcher: running (pid N)`
head -1 <project home>/dispatch.pid        # N, not the old pid
```

**Do not read the pane to decide whether it started.** `herdr pane read` returns what is
painted, which includes the dead dispatcher's last frame still on screen — a masthead reading
`dispatcher running · pid <old>` is that old frame, not a live claim. The masthead prints
`std::process::id()`, so it can only ever name its own process. The lock file and `queue
list` are the evidence; the pane is not.

**Never switch backend with a live lane.** The backends cannot see each other, and a switch
mid-run duplicates agents into the same worktree.

## Reporting

Say which pull requests merged, and name every conflict you resolved and how. A conflict
resolution is a decision made on the human's behalf; it is the one part of this pass they
cannot see from the log.

Name any red check as well, including one below a tip that you merged anyway, and say what
made it safe. For a red tip you fixed, say what was broken and what you changed on the
branch — the fix is a commit the task's author never wrote, and it is theirs to disagree with.
Say which pull requests you left open, and whether it was because the fix would have dropped
behaviour or because the failure was the task's premise, so nobody has to work out from the
open list whether they were missed or refused.

Then say what `main`'s own dispatched run did, by run id, and name anything you fixed to get it
there. That run is the only statement anybody has that the merged tree is good; a report that
ends at the push is claiming something it never checked.

## What has bitten before

- **The pass ended at the push, and `main` stayed red.** `ci.yml` carries no `push:` trigger,
  so the merges landed and nothing ran against them; the next thing to look at `main` was the
  03:17 schedule, which had already been failing for two days. What it was failing on was
  `dress-rehearsal`, and no merged pull request had anything to do with it: v0.4.0 was tagged and
  published, `Cargo.toml` moved past it, and `scripts/e2e/fixtures/0.4.0/` — the closing step of
  `docs/releasing.md` — was never scaffolded, so `scripts/e2e/suites/upgrade.sh` began asking for
  it exactly as it is designed to. Two lessons, and the second is the one that generalises. A
  merge pass owes `main` a green run of main's own gate, dispatched rather than assumed. And the
  red it finds there will often have nothing to do with the pull requests it merged, because
  main's gate covers a release rehearsal, a nightly tier and a set of advisories that no branch
  ever runs — so "none of my merges caused this" is not a reason to leave it.
- **The stacked pull request looks conflict-free and is not.** A stacked branch reports
  `CLEAN` against its own base while its tip conflicts badly with `main`. `mergeStateStatus`
  answers a question about `baseRefName`, so on a stack it is answering the wrong one.
- **A red check below the tip of a stack.** A branch that drops a feature can go red on the
  job that still tests it, while the branch above it is the one that deletes the job. The
  failure is real and merging past it is still right, because the tip is what lands. Read the
  job before deciding that, and never generalise it into ignoring red on a tip.
- **Merged before the checks came back.** A pull request that has just been opened shows most
  of its checks still running, and a rollup read at the survey goes stale within minutes.
  Re-read it at the tip, with `--watch`, right before the merge.
- **An e2e suite that hangs instead of failing, and reads as a slow job.** `dispatch` that
  finds the lock held used to draw a read-only board and poll forever, and that branch sat above
  the refusals in `run` — so a suite asserting on a refusal, with a dispatcher `drive` left
  running, never reached the thing under test and never returned. It burned a whole 45-minute
  job timeout on #174 before anybody called it stuck. That mechanism is gone now — a held lock
  prints two lines and exits 4 at once — so a suite in that shape today fails on its assertion
  rather than hanging. The two lessons underneath it are still general: a suite case that
  expects a refusal needs `dispatcher_stop` in front of it, and a step running five times its
  usual duration is hung, not slow — reproduce it locally rather than waiting for a log the
  forge will not serve until the job ends.
- **`main` moved between the survey and the merge.** Other things push here. If the merge
  behaves strangely, `git fetch origin` and check that `main` is still where step 2 left it.
- **A hunk that references a deleted binding.** `git merge` keeps whichever side you tell it
  to, including a `HEAD` side naming a variable the other side removed. This compiles as an
  error, so the build catches it — but only if you build before you push.
- **The installed binary lies about the merge.** Every check that spells `spoolway` runs the
  installed build, not the merged one. Use `./target/debug/spoolway` for anything you are
  reading as evidence, right up until step 9 replaces the install.
- **The tests pass and the frame is wrong.** Column arithmetic is pinned by a handful of
  tests, not by all of them. Two branches that each move a column can pass every test and
  still produce a board where a heading sits over the wrong values.
- **A clean auto-merge that lost a side anyway.** Two branches can edit one file without
  git ever raising a conflict, and still leave a tree where one side's intent is gone. After
  a merge with no conflicts, list the files both sides touched — `comm -12` over the two
  `git diff --name-only` outputs — and check that every line each side added is still in the
  merged file. It is a one-line loop and it is the only thing standing between a silent drop
  and a push.
- **A heredoc that ran the command it was describing.** Writing prose into a file with
  `python3 - <<PY` — the delimiter unquoted — hands the whole body to the shell for
  expansion first. Backticks around a command name in that prose are not decoration; they
  are command substitution, and `` `spoolway dispatch` `` in a sentence *starts a
  dispatcher*. It did: three lanes, three worktrees, three panes and three live agents, from
  a call whose visible purpose was to add a description to a task document. The shell gave no
  hint — the call simply hung, because the dispatcher it had started does not return.
  Quote the delimiter, always: `<<'PY'`. Nothing inside then means anything to the shell.
  The general form is that this pass writes a lot of text containing command names, and text
  containing command names is dangerous in exactly one place, which is an unquoted heredoc.
- **The second `ctrl-c` killed the graceful stop.** Three spellings were tried against
  `herdr pane send-keys` to find the one it accepted — `c-c`, `C-c`, `ctrl+c`. All three
  landed. The first asked the dispatcher to stop, and the second hit while `sweep_on_stop`
  was still harvesting transcripts, restoring `SIG_DFL` and killing it where it stood: no
  spend banked for the live lane, no launch counter forgiven, and `dispatch.pid` left naming
  a dead process. The general form: a key-sending API that reports nothing on success gives
  no way to tell "wrong spelling" from "sent", so probing spellings sends the key as many
  times as you probe. Look the spelling up, send it once, and verify by the effect.
- **`sync` wanted to undo a commit, and taking it whole would have.** Step 10 is not
  automatic. A merge that installs an older binary than the project's own control plane makes
  `sync` offer to put back a setting somebody deliberately retired — here `dispatch.interval`,
  removed hours earlier ahead of the task that retires it in the binary. Read the dry run
  file by file and take the ones that are the new binary's to write. A setting the project
  moved past is the project's, not the binary's, and `doctor` reporting it as drift is
  expected until that task lands.
- **`spoolway sync` was skipped, so `doctor` complains for weeks.** The drift step 10 clears
  looks like something wrong with the project. It is the merge's own doing, and it appears on
  every pass that brings in a new default.
