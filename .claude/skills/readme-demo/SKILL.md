---
name: readme-demo
description: Refresh everything the README shows of spoolway in one go - check the README for stale text, build the "shop" demo project, render the README's TUI screenshots, let a person record a fresh demo GIF or video, turn the recording into the GIF, and remove the whole demo afterwards. Use when someone wants to re-record the README demo, refresh the demo GIF or video, retake the README screenshots or the eval screenshot, check the README for anything outdated, or clean up the demo project.
---

# README demo

The README shows a demo GIF and five screenshots. They all come from a small demo project
called `shop`. This skill checks the README text for anything stale, builds that project,
renders the screenshots, opens the project in herdr for a person to record, turns the
recording into the GIF, and removes the project completely at the end.

The person records the GIF. Do not record, queue tasks, or start the dispatcher for them: the
video shows them queueing the two groups and pressing `enter` on the dispatch tab. The
screenshots are yours to render, from a private tmux server the person never sees. When nobody
records, `scripts/headless-dispatch.sh` queues and dispatches for the dispatch screenshot.

## What the README shows

| File | What it shows | How it is made |
|---|---|---|
| `docs/assets/spoolway-demo.gif` | The person queueing both groups, starting the dispatcher and walking the tabs | The person records; `scripts/make-gif.sh` converts it |
| `docs/screenshots/readme/queue.png` | The queue tab with both groups ticked | `scripts/screenshots.sh` |
| `docs/screenshots/readme/routines.png` | The routines tab on `nightly`, with its job | `scripts/screenshots.sh` |
| `docs/screenshots/readme/eval.png` | The eval tab's lanes table from the seeded ledger | `scripts/screenshots.sh` |
| `docs/screenshots/readme/plan.png` | The top of a plan page `/spoolway-plan` writes | `scripts/screenshots.sh`, through `scripts/plan-page.py` |
| `docs/screenshots/readme/dispatch.png` | The dispatch board with two lanes running | `scripts/screenshots.sh --dispatch`, during the person's run |

The docs pages under `docs/` embed the same files, so a retake keeps every name. Never write
into `docs/screenshots/test/`: that folder belongs to the `impl_ui` pipeline's captures.

## What the demo contains

- A Node app at `~/demo/shop` with a cart, a checkout and a login, and 8 passing tests.
- Three pipelines: `impl`, `impl_fast` and `impl_tdd`. Every agent step runs Claude with
  `claude-haiku-5-5` in auto mode, so a live run is quick and cheap. Each pipeline ends with
  `npm test`.
- Slots: Claude 2.
- Two groups in pending, not queued. Each group is one dependency chain, because spoolway
  refuses a group with two unchained tasks.
  - `cart`: `cart-empty-state` → `cart-totals` → `cart-discounts`
  - `checkout`: `auth-verify` → `checkout-charge` → `checkout-receipt`
- Three routines: `nightly` (2 tasks), `weekly-deps` and `release-notes`.
- Two jobs: `nightly-audit` in the repo, and `weekly-deps` in the project's home.
- Three weeks of made-up runs in the ledger, so the eval tab is never empty. Setup writes
  them with `scripts/fake-eval.py`: 98 seeded lines in `<home>/usage.jsonl`, with two
  versions of `impl`, one settled trial and three directory sessions.

## Steps

The scripts live in `scripts/` next to this file. Run them from any directory.

1. Check the binary. Compare `spoolway --version` with `version` in the repo's `Cargo.toml`.
   The board prints the version, and every image shows the installed binary's look, so an old
   binary makes the new images stale at once. If it is behind, tell the person and ask whether
   to update first. `spoolway update` installs the newest release. `cargo install --path .` in
   the repo installs an unreleased version. A dispatcher the person already runs keeps the old
   binary until they restart it.
2. Check the README for stale text, before building anything. Read `README.md`, then check it
   against the source at HEAD, not against the installed binary's help. Split the work across
   three read-only subagents in parallel, one per part: the top through Quick start, the
   pipeline example through the issue tracker, and the config block with the eval section.
   Each part checks:
   - every key, tab name, command, flag, path and skill name against `src/`, `assets/` and
     `.claude/skills/`
   - every config key, default and comment against `src/config.rs` and what `spoolway init`
     writes
   - every example against what `init` ships, and every model id against
     `assets/model-prices.json`
   - every link and `#anchor` against the target file's headings
   - the `CHANGELOG.md` sections since the README last changed, for anything it now misses
   - every image in the table above against the current tab strip and key lines

   Show the person an overview first: one short table with each finding, its README line and
   how sure it is. Then give the details. Do not edit the README until the person says which
   findings to fix.
3. If `~/demo` already exists, ask whether to run `scripts/teardown.sh` first. Setup refuses
   to run over it.
4. Run `scripts/setup.sh`. Its last line is the project's home, `~/.spoolway/shop-<id>`.
5. Check that the eval data is there with `spoolway -C ~/demo/shop eval`. It should list
   `impl`, `impl_fast` and `impl_tdd`. Setup already ran `scripts/fake-eval.py`, so do not
   run it again: it appends, and a second run doubles every row.
6. Run `scripts/screenshots.sh`. It writes `queue.png`, `routines.png`, `eval.png` and
   `plan.png` into `docs/screenshots/readme/`. Run it before the person queues anything,
   because the queue tab is empty once both groups are queued. The plan page is the shipped
   `spoolway-plan` template filled with the cart plan, written to the demo's plans folder.
   Look at each image before going on. When nobody records, run
   `scripts/headless-dispatch.sh` and then `scripts/screenshots.sh --dispatch`, and skip to
   teardown.
7. Run `scripts/open.sh`. It opens a new Ubuntu window the same way the person's pinned
   taskbar icon does, running a separate herdr session called `demo`. None of the person's own
   workspaces show in the recording. It names that session's workspace `shop` and starts bare
   `spoolway` there. Never open the demo in a hidden tmux session for the person: they need to
   see and drive it.
8. Tell the person it is ready, and that the dispatcher is not started. Ask them to leave the
   dispatcher running and the demo pane on `[1] DISPATCH` when they stop recording, so the
   dispatch screenshot can be taken.

## When the person is done

1. Take the dispatch screenshot while lanes still run: `scripts/screenshots.sh --dispatch`.
   It only reads the demo pane. If it says the pane is not on a running board, ask the person
   to switch back to the dispatch tab.
2. Turn the recording into the GIF. It is usually the newest `.mp4` on the Windows desktop,
   at `/mnt/c/Users/<windows user>/Desktop/` from WSL. Look at a few frames from its start and
   end first, in case it needs trimming. Then run `scripts/make-gif.sh <video>`, which writes
   `docs/assets/spoolway-demo.gif`. The README embeds a GIF because GitHub does not play a
   video file committed to the repo.
3. Look at every new image, and check that the board's version matches step 1. A retake needs
   the demo, so do this before teardown.
4. Run `scripts/teardown.sh`. Use `--dry-run` first if anything else might be under `~/demo`.

Teardown removes:

- the `demo` herdr session, which closes its window
- every herdr workspace with a pane in the checkout or the project's home, matched by
  directory and not by label, because the real spoolway project's lanes use the same labels
- every process still running in either directory
- `~/demo`, `~/.spoolway/shop-<id>` and `~/.spoolway/logs/shop-<id>.log`
- the Claude and pi session folders the lanes wrote for the demo

It ends with `demo removed` only after checking that none of those remain. If it prints
`still there` or `still running`, look at what is left before removing anything by hand.

## Keeping this current

Setup is written against the spoolway binary on `PATH`. When a run fails, fix the script
rather than working around it by hand. Recent examples:

- `dispatch.default_pipeline` no longer exists.
- `spoolway config set` cannot create a new agent profile or model table. Append the table to
  `config.toml`, then run `spoolway sync` to tidy its comments.
- Use Haiku 5.5, not Haiku 4.5. Haiku 4.5 has no auto mode, so Claude falls back to manual
  mode and every lane stops on approval prompts. Do not work around that with blanket shell
  permissions for the lanes.
- spoolway's shipped price table has no Haiku 5.5, so setup writes its price and window into
  the demo's `config.toml`. Drop that table once `assets/model-prices.json` knows the model.
- Claude asks "Do you trust this folder?" in any untrusted project, and a lane cannot answer.
  Setup trusts `~/demo/shop` in `~/.claude.json` with `scripts/claude-trust.py`. Task worktrees
  count as part of that checkout. Teardown removes the entry again.
- A group must be one dependency chain.
- Open the window with `cmd.exe /c start "" "C:\Program Files\WSL\wsl.exe" ...`, like the
  taskbar icon. `wt.exe -p Ubuntu` opens Windows Terminal's Ubuntu profile, whose font looks
  wrong to the person.
- Start herdr in an interactive zsh (`zsh -ic`). A login shell (`-lc`) does not read
  `~/.zshrc`, where herdr's `PATH` entry lives, so it fails with `command not found`.
- `open.sh` launches the window from a background subshell. Its redirect sits outside the
  subshell, or the subshell keeps the script's output open and a caller piping it, such as
  `open.sh | tail`, waits forever.
- `freeze`'s own PNG writer crashes with a Go stack overflow on these screens. `screenshots.sh`
  has freeze write an SVG and renders the PNG with headless Chrome.
- The eval screenshot needs 160 columns and 15 rows. Fewer columns cut off `TIME`, and fewer
  rows scroll the table, which then shows `↓ 2 below`.
- ffmpeg is not installed on this machine. `make-gif.sh` falls back to the static ffmpeg inside
  the `imageio-ffmpeg` Python package, through `uv`.
- A shell inside the person's herdr carries `HERDR_ENV`, `HERDR_SOCKET_PATH` and the pane ids.
  A herdr started with them never comes up, so `headless-dispatch.sh` starts its tmux without
  them. `open.sh` has no such problem: Windows starts its window with a clean environment.
- The plan screenshot is 1100 by 640 pixels, doubled. That ends just below Context's folder
  tree and above the Decisions heading.
