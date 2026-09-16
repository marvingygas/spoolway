# Releasing spoolway

A release is a `v*` tag. The `release` pipeline cuts it. Queue the routine, approve the
release notes at the gate, and the pipeline does the rest — including repairing `main`
itself if the readiness suite comes back red.

```mermaid
flowchart LR
  R[ready] -->|red| X[fix] --> R
  R -->|green| A[preflight] --> B[notes] -->|gate: you approve| C[publish] --> V[released]
  C --> D[release commit on main] --> E[rehearsal run] --> F[tag] --> G[npm + GitHub release]
```

| Step | What it does |
|---|---|
| `ready` | Runs the whole local gate and a fresh nightly CI run on the candidate. Verifies only — it never edits. |
| `fix` | Repairs whatever `ready` found, in one pull request, and merges it to `main` itself. Product code included. |
| `preflight` | Checks `main` is clean and green. Lists every change since the last tag. Proposes the version. |
| `notes` | Writes one changelog section. Stops at a gate until you approve it. |
| `publish` | Commits the version bump and the section, rehearses the workflow, tags, and checks what npm and GitHub received. |
| `released` | `scripts/release-verify.sh`. Proves the tag, the seven packages, the archives and the published body exist. A command step, so nothing can be credited with it — see below. |

`released` is not ceremony. `publish` is an agent step, and under
`unattended.skip_blocked_lane` a cleared block on an agent step carries the task one step
*past* it — so a release could reach `done` with nothing published. A command step is
never carried past, so `released` is what makes `done` mean released.

## Cutting a release

1. Open `spoolway queue`, press `r`, and queue `release-spoolway`.
2. When the task pauses at `notes`, read the section and approve it or send it back.
3. Wait for `done`. The task reports the tag, the workflow runs, the registry versions and the
   result of a real install.

The routine is `.spoolway/routines/release-spoolway/release-spoolway.md`. The prompts are
`.spoolway/prompts/release-*/PROMPT.md`. The workflow is `.github/workflows/release.yml`.

## What ships

- Six platform binaries, packed into six npm platform packages plus the `spoolway` wrapper.
- A GitHub release with six archives and `SHA256SUMS`.
- The changelog section, as the release body and inside every binary (`spoolway whats-new`).

`Cargo.toml` is the only version source. `CHANGELOG.md` holds one section per version and its
contract is written at the top of the file.

## Version rule

While the major version is `0`: a change in user-facing shape bumps minor. Fixes and internal
changes alone bump patch.

## Commands the pipeline runs

Local gate, in this order:

```sh
cargo fmt --check
cargo deny check advisories
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --all-targets --locked
cargo build --release --locked
./target/release/spoolway pipeline check
```

Release commit. Only `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md` change, with subject
`chore(release): v<version>`:

```sh
cargo check --offline                              # refreshes the lock entry
cargo test --locked release_notes::tests
cargo build --release --locked && ./target/release/spoolway whats-new
git push origin main
```

Rehearsal, on the release commit, with publication off:

```sh
gh workflow run release.yml --ref main -f publish=false
gh run list --workflow release.yml --event workflow_dispatch --branch main --commit <release-sha>
gh run view <run-id> --json event,headSha,status,conclusion,jobs
gh run watch <run-id>
```

Tag, only after the rehearsal is green on that exact SHA:

```sh
git tag v<version> <release-sha>
git push origin refs/tags/v<version>
```

Verify:

```sh
npm view <package>@<version> version               # each name in npm/targets.json, plus spoolway
gh release view v<version> --json body --jq .body   # must equal the approved section
cd "$(mktemp -d)" && npm install spoolway@<version> && ./node_modules/.bin/spoolway --version
```

## The fixture the nightly suite asks for

`scripts/e2e/suites/upgrade.sh` runs a project a *past* release actually wrote through the
new binary, from `scripts/e2e/fixtures/<version>/.spoolway/` — a tree scaffolded by that
version's own binary, at that version's own tag. It cannot be produced before the tag, so the
suite exempts the version `Cargo.toml` names and asks for every older `CHANGELOG.md` section.
A release is therefore never blocked by its own missing fixture, and the next bump turns the
exemption into a failing check.

Scaffold the released version's fixture once the tag is out — before the next bump, which is
when the suite starts asking:

```sh
npx spoolway@<version> init                        # in a scratch git repo
npx spoolway@<version> config set housekeeping.retention_days 45
```

Copy that `.spoolway/` to `scripts/e2e/fixtures/<version>/`, hand-add one line of prose below
the pipeline file's generated key block, and commit it. The suite's own header says what each
part is for.

## When it fails

- A red rehearsal leaves an untagged candidate. Fix the cause, then rehearse again. A product
  change needs a new preflight and a new approval.
- If `main` moves before the tag, the publisher removes or reverts its release commit and the
  task goes back to `preflight`.
- A partial publish is retried with `gh run rerun <run-id> --failed`. Published npm versions
  are never replaced. A successful release tag is never moved.
