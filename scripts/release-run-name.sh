#!/usr/bin/env bash
# Print the run's name, as scripts/release-anchor.sh recorded it, for the
# release pipeline's branch names: `release/$(scripts/release-run-name.sh)`.
# Fails, naming the missing file, when the anchor step has not run, so a step
# never builds a branch prefix from an empty name.
set -euo pipefail

file=.release-run/run
[ -s "$file" ] || { echo "release-run-name: $file is missing or empty — run scripts/release-anchor.sh from the release worktree first" >&2; exit 1; }
cat "$file"
