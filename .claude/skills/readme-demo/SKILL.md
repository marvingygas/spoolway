---
name: readme-demo
description: Set up the "shop" demo project that the README GIF records, so a person can record a fresh GIF or video of the spoolway TUI, fill the eval tab with made-up runs for a screenshot, and remove the whole demo afterwards. Use when someone wants to re-record the README demo, refresh the demo GIF or video, retake the eval screenshot, or clean up the demo project.
---

# README demo

The README's demo GIF (`docs/assets/spoolway-demo.gif`) is recorded by a person on a small
demo project called `shop`. This skill builds that project, opens it in herdr, and removes it
completely when the recording is done.

The person records. Do not record, queue tasks, or start the dispatcher for them: the video
shows them queueing the two groups and pressing `enter` on the dispatch tab.

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

## Steps

The scripts live in `scripts/` next to this file. Run them from any directory.

1. If `~/demo` already exists, ask whether to run `scripts/teardown.sh` first. Setup refuses
   to run over it.
2. Run `scripts/setup.sh`. Its last line is the project's home, `~/.spoolway/shop-<id>`.
3. If the person wants an eval screenshot, run `scripts/fake-eval.py <home>`. It appends about
   90 seeded ledger lines to `<home>/usage.jsonl`. That gives the lanes table three weeks of
   runs, two versions of `impl`, one settled trial and three directory sessions. Check it with
   `spoolway -C ~/demo/shop eval`.
4. Run `scripts/open.sh`. It opens a new Ubuntu window the same way the person's pinned
   taskbar icon does, running a separate herdr session called `demo`. None of the person's own
   workspaces show in the recording. It names that session's workspace `shop` and starts bare
   `spoolway` there. Never open the demo in a hidden tmux session for the person: they need to
   see and drive it.
5. Tell the person it is ready, and that the dispatcher is not started.

## When the person is done

Run `scripts/teardown.sh`. Use `--dry-run` first if anything else might be under `~/demo`.

It removes:

- the `demo` herdr session, which closes its window
- every herdr workspace with a pane in the checkout or the project's home, matched by
  directory and not by label, because the real spoolway project's lanes use the same labels
- every process still running in either directory
- `~/demo`, `~/.spoolway/shop-<id>` and `~/.spoolway/logs/shop-<id>.log`
- the Claude and pi session folders the lanes wrote for the demo

It ends with `demo removed` only after checking that none of those remain. If it prints
`still there` or `still running`, look at what is left before removing anything by hand.

If the person recorded a new video, it is usually on the Windows desktop, at
`/mnt/c/Users/<windows user>/Desktop/` from WSL. The README embeds a GIF because GitHub does not play a video
file committed to the repo.

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
