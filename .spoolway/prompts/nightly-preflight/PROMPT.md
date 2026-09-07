# Nightly end-to-end preflight

## What you are looking at

You own the command-heavy first two sections of the nightly routine named in the task's References:
the headless suites and the Windows test that only happens here. Read that source routine in full
before acting. The next lane owns the watched plan runs; your findings and handoffs are its input.

This lane itself proves a local worker can operate the routine. The live Codex tier separately
proves the Codex adapter against the exact local model named in `~/.codex/config.toml`.

## How to do it here

1. Follow sections 1 and 2 of the source routine exactly, including the nightly, cloud-transcript,
   live-Codex, settings-map, and Windows checks. The cloud tier is part of every pass in this lane:
   one definite pass is worth its small spend.
2. Build the release binary first. Do not copy over the installed binary: the outer dispatcher is
   executing it. Select this worktree's release build explicitly wherever the routine selects the
   binary under test.
3. For the live Codex tier, use the config's exact model name. Check the local router health first.
   If it is down, start it with `~/dev/tools/local-llm/llm.sh`, remember that you started it, and stop
   it afterwards with `llm.sh stop`.
4. Actually run the Windows tests; compiling that target is not enough:

       cargo test --target x86_64-pc-windows-gnu --all-targets --locked

5. Read the settings map. Count every bare `no case`, compare it with the baseline of 20
   from 2026-08-28, and hand off the count plus every uncovered key. Also diff
   `scripts/e2e/runtime/end-to-end.yml` against `assets/pipelines/default.yml` and hand off drift.
6. Diagnose every failure before changing anything. Fix only a demonstrated mechanical defect:
   a stale assertion, missed rename, or stale coverage claim. Leave a design decision as a precise
   handoff with the file, reasoning, and proposed approach. If you changed the tree, rebuild and
   rerun the affected tier.
7. Hand off each tier's result, whether the router was started and stopped, the Windows result,
   the `no case` count, any pipeline drift, and every file you changed.

## Never

- Never copy, install, or replace the dispatcher binary while this dispatcher is running.
- Never skip the live or cloud tier silently. A missing binary, model, credential, router, or
  Windows interop is a block or a named finding.
- Never weaken or delete a correct check to make a tier green.
- Never leave a router running if this lane started it.
- Never start the watched plan runs. They belong to the next lane.
