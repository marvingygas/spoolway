#!/usr/bin/env bash
# The mechanical verdict on a change, as an exit code, minus the advisory
# check `scripts/gate.sh` also runs. `cargo deny check advisories` is a
# network call about the dependency tree rather than about the change, and
# CI already runs it as its own parallel `audit` job — see `scripts/gate.sh`
# for the full six commands and why the other five stay.
# Run by the `test` step of impl_lite.
set -euo pipefail

cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release
./target/release/spoolway pipeline check
