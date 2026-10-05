#!/usr/bin/env bash
# The mechanical verdict on a change, as an exit code.
# Run by the `test` step of impl, impl_tdd, impl_ui, impl_fast and bugfix.
set -euo pipefail

cargo fmt --check
cargo deny check advisories
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release
./target/release/spoolway pipeline check
