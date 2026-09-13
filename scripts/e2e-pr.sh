#!/usr/bin/env bash
# The end-to-end suites, on the `pr` tier, against the binary this checkout
# just built. Run by the `suite` step of impl, impl_tdd, impl_ui and bugfix —
# the last task of a chain only, per that step's own `last: true`.
set -euo pipefail

SPOOLWAY="$PWD/target/release/spoolway" scripts/e2e/run.sh --tier pr
