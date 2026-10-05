#!/usr/bin/env bash
# The mechanical verdict on a change, as an exit code.
# Run by the `test` step of impl, impl_tdd, impl_ui, impl_fast and bugfix.
set -euo pipefail

# `target/debug` is a symlink into a cargo target every worktree shares
# (`link_shared_target` in src/mux.rs). Another lane's build can overwrite this
# crate's test binaries there while this gate runs, so its tests would run that
# worktree's code. The debug steps build into a target of this worktree's own;
# the release build below stays in `target/release`, where `suite` and
# `handover` look for it.
export CARGO_TARGET_DIR="$PWD/target/gate"

cargo fmt --check
cargo deny check advisories
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked

unset CARGO_TARGET_DIR
cargo build --release
./target/release/spoolway pipeline check
