# spoolway

A local-first agent pipeline, in Rust. This repo builds spoolway **and is developed with it**,
so `.spoolway/` here is a live control plane, not a fixture.

## Two of almost everything

| Product, shipped in the binary | This project's own installation |
|---|---|
| `assets/prompts/<name>/PROMPT.md` | `.spoolway/prompts/<name>/PROMPT.md` |
| `assets/pipelines/*.yml` | `.spoolway/pipelines/*.yml` |
| `assets/tasks/*.md` | `.spoolway/templates/tasks/*.md` |
| `assets/skills/claude/*` | `.claude/skills/*` |


## Releasing

spoolway ships on npm as `spoolway`, a wrapper package that resolves one
`@spoolway/<platform>` package holding a single binary. The same six binaries are attached to
the GitHub release as archives, for anyone installing without a package manager. There is no
crates.io release and no `curl | sh` installer.

Cutting a release is a tag. Nothing is published by hand.

```
# bump [package] version in Cargo.toml -- the only place a version is written
git commit -am "chore: release 0.2.0"
git tag v0.2.0 && git push origin main --tags
```

`.github/workflows/release.yml` then builds all six targets, refuses if the tag disagrees with
`Cargo.toml`, publishes the platform packages **first** and the wrapper **last**, and attaches
the archives. To rehearse without touching the registry, run it from the Actions tab with
`publish` unchecked.

Version numbers are semver, and 0.x means a minor bump may break things.

A release that changes a shipped prompt needs nothing extra. `spoolway update` takes a file
back only when the project's copy is byte-for-byte what spoolway shipped, so a prompt a
project has written in is left alone and reported rather than overwritten. The fingerprint
machinery in `src/skeleton.rs` is for blocks inside a file a project owns the rest of, and
`skeletons()` currently registers none.

`scripts/build-npm.mjs` assembles the packages and fails loudly on the two things that would
otherwise ship broken: the platform table the wrapper shim duplicates drifting from
`npm/targets.json`, and a version that is not `Cargo.toml`'s. Platform packages must never
declare `exports` -- the wrapper resolves them by subpath.

## Installing a build

The dispatcher runs the binary on `PATH`, at `~/.local/bin/spoolway`. Every lane resolves
`spoolway report` through `PATH` too, so the build a lane reports to is whatever is installed
when it runs, not whatever started the dispatcher.

```
cargo build --release              # freely, any time
spoolway queue list                # nothing in flight?
cp target/release/spoolway ~/.local/bin/
```

**Never that `cp` while a dispatcher runs.** `cp` writes through the inode the running
dispatcher is executing, and a half-written binary is a dispatcher that dies mid-pass — and
lanes launched after it pick up a build nobody meant to be running yet. Install between runs,
then restart. Same reason a lane may *build* the binary but never install it.

Do not update this file without being asked.
