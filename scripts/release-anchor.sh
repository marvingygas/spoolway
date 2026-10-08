#!/usr/bin/env bash
# Record the two facts the rest of the release pipeline needs from the worktree
# it runs in, so no later step has to read them out of spoolway's environment.
# Run by the `anchor` step, first in the pipeline. A person walking a release by
# hand runs it once, from the freshly cut worktree, before anything else.
#
# `.release-run/base-commit` is `git rev-parse HEAD`: the commit `main` stood at
# when this worktree was cut. scripts/release-verify.sh and
# scripts/release-publish.sh anchor the candidate to it. It must be taken now.
# `git merge-base HEAD origin/main` agrees only until the branch is rebased,
# which is the case the anchor exists for.
#
# `.release-run/run` is the worktree's current branch with a leading `task/`
# removed. It is the name the `release/`, `release-fix/` and `fixture/` branches
# are suffixed with, and what scripts/release-await-merge.sh waits on.
#
# The directory is listed in .gitignore, so it never reaches a pull request.
# Files that already exist are kept: a re-run after a rebase would otherwise
# record the rebased HEAD and quietly move the anchor.
set -euo pipefail

dir=.release-run

mkdir -p "$dir"

if [ ! -s "$dir/base-commit" ]; then
  git rev-parse --verify HEAD > "$dir/base-commit"
fi

if [ ! -s "$dir/run" ]; then
  branch="$(git branch --show-current)"
  [ -n "$branch" ] || { echo "release-anchor: HEAD is detached, so there is no branch to name the run after" >&2; exit 1; }
  printf '%s\n' "${branch#task/}" > "$dir/run"
fi

echo "release-anchor: base $(cat "$dir/base-commit"), run $(cat "$dir/run")" >&2
