#!/usr/bin/env bash
# The mechanical verdict on a change, as an exit code.
# Run by the `test` step of impl, impl_tdd, impl_ui, impl_fast and bugfix.
set -euo pipefail

# `target/debug` is a symlink into a cargo target every worktree shares
# (`link_shared_target` in src/mux.rs), and Cargo judges freshness by mtime
# against paths relative to the package. A test binary another worktree built
# later than this checkout's sources were written is taken as fresh here, and
# its tests run against that worktree's code and CARGO_MANIFEST_DIR. Touching
# the crate root recompiles this crate from this checkout; dependencies stay
# shared.
touch src/main.rs

cargo fmt --check
cargo deny check advisories
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release
./target/release/spoolway pipeline check
