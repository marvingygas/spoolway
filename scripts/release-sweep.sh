#!/usr/bin/env bash
# Delete the scratch branches a release left on origin. Run by `ship` once the
# release is verified, and safe to run by hand: `scripts/release-sweep.sh 0.14.0`.
#
# GitHub deletes a pull request's head branch when it merges, so
# `release/v<version>` and `fixture/v<version>` clean up after themselves. A
# branch pushed only to rehearse a fix on another runner has no pull request,
# and nothing deleted it: v0.14.0's macOS recovery left four
# `release-fix/v0.14.0-macos*` branches and v0.15.0's left
# `rehearse/v0.15.0-macos`, all on origin until someone noticed.
#
# A branch is swept when its name carries the released tag, no open pull
# request has it as head, and every commit on it is already on `origin/main`
# (`git cherry`, so a rebase-merged copy counts). Anything else is named and
# kept. Never fails the release: a branch it cannot judge is kept.
set -uo pipefail

say() { printf 'release-sweep: %s\n' "$*" >&2; }

repo="$(git remote get-url origin)"
version="${1:-$(awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}' Cargo.toml)}"
[ -n "$version" ] || { say "could not read the package version from Cargo.toml"; exit 0; }
tag="v$version"

git fetch --quiet --prune origin || { say "could not fetch origin; nothing swept"; exit 0; }

# `v0.14.0` must not match `v0.14.01`, so the tag ends at a non-digit.
pattern="(^|[/_-])${tag//./\\.}([^0-9]|$)"
swept=0
while read -r b; do
  case "$b" in "release/$tag" | "fixture/$tag") continue ;; esac
  if ! open="$(gh pr list --repo "$repo" --head "$b" --state open --json number --jq 'length')"; then
    say "kept $b: could not ask GitHub for its pull requests"; continue
  fi
  if [ "$open" != 0 ]; then
    say "kept $b: a pull request from it is still open"; continue
  fi
  if ! unmerged="$(git cherry origin/main "origin/$b")"; then
    say "kept $b: could not compare it with origin/main"; continue
  fi
  if grep -q '^+' <<<"$unmerged"; then
    say "kept $b: it has commits that are not on main"; continue
  fi
  if git push --quiet origin --delete "$b"; then
    say "deleted $b"; swept=$((swept + 1))
  else
    say "kept $b: the delete was refused"
  fi
done < <(git for-each-ref --format='%(refname:lstrip=3)' refs/remotes/origin | grep -E "$pattern")

say "swept $swept scratch branch(es) for $tag"
