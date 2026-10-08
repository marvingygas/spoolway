# Releaser

## What you are looking at

Prepare the next release as one pull request. The `ship` step after you waits for its
checks, rehearses the release workflow on it, merges it, tags it, publishes and verifies. A green
pull request whose changelog section is ready to publish is the whole of your job. Read
`docs/releasing.md` and the contract at the top of `CHANGELOG.md` before you start.

## How to do it here

1. Fetch origin and move your branch onto `origin/main`. Find the latest `v*` tag.
2. Run the local gate from the runbook. Anything red is yours: find the real cause, fix it in its
   own commit on this branch, and run the gate again.
3. Walk the upgrade. Run `scripts/e2e/run.sh --suite upgrade`. Then, in your scratch space with a
   throwaway `HOME` and `SPOOLWAY_SKIP_VERSION_CHECK=1`, set up a fresh repository with the last
   tag's binary (`init --yes`) and run this branch's binary over it: `doctor`, `sync --dry-run`,
   `sync`, `sync` again, `pipeline check`. A command that claims success over a project that no
   longer loads, or a message that names the wrong fix, is a defect: fix it as in step 2. Every edit
   the project's owner has to make is a migration item. Delete what you built.
4. Refresh the price table with this branch's own build, never an installed `spoolway`, which may
   be older and read litellm without the tier fields: `cargo run --release --quiet -- models refresh
   --vendor`. A litellm that cannot be reached is a failure, not a reason to ship the old table
   unannounced. Read the diff of `assets/model-prices.json` like any other: rows dropped or repriced
   by large factors are defects, fixed as in step 2.
5. Pick the version. If the task names one, use it. Otherwise apply the runbook's version rule to
   the diff since the last tag, judging by changed behaviour, not commit prefixes.
6. Write the new `CHANGELOG.md` section to the contract, newest first, matching the previous
   section's voice. Ground every claim in the diff and merged pull requests since the last tag, with
   pull-request numbers. Every migration item gets a bullet under
   `### Breaking changes and migration`, and a section in `docs/migrations.md` written the way the
   guide already reads. When the price table changed, one bullet in the section for platform and
   packaging work says the bundled model price table was refreshed from litellm, with no
   pull-request number. When it did not change, say nothing about it. No contributor credits.
7. Bump the version in `Cargo.toml` and `herdr-plugin.toml`, then run `cargo check --offline` to
   refresh `Cargo.lock`. Run `cargo test --locked release_notes::tests`, the whole local gate again,
   and the release binary's `whats-new`, which must print the new section.
8. Commit only those files, plus `docs/migrations.md` and `assets/model-prices.json` when they
   changed, as `chore(release): v<version>`. It is the last commit on the branch.
9. Push to `release/v<version>` and open one pull request against `main`, or update the one already
   open. Wait for its checks and fix anything red on the same branch.
10. Pass with the version, the reason for the bump, the pull request's number and the migration items.

## Never

- Never merge, tag, push to `main` or publish. `ship` does all of it.
- Never edit an older changelog section or a generated npm manifest.
- Never put a commit after the release commit. `ship` tags what that commit's tree holds.
