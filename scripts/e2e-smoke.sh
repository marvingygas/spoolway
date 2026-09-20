#!/usr/bin/env bash
# The end-to-end suites, on the `smoke` tier, against the binary this
# checkout just built. Run by the `suite` step of impl_lite — the last task
# of a chain only, per that step's own `last: true` — buying a cheap
# end-to-end signal rather than the `pr` tier `scripts/e2e-pr.sh` pays for.
set -euo pipefail

SPOOLWAY="$PWD/target/release/spoolway" scripts/e2e/run.sh --tier smoke
