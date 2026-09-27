#!/usr/bin/env bash
# Wait for a person to merge the pull request the preceding release-reviewer
# step left open, so the release pipeline can carry on by itself instead of
# stopping on a merge Claude Code's auto-mode permission layer refuses. Run by
# the `await-fix`, `await-release` and `await-fixture` steps.
#
# Takes the branch prefix the producing step already uses: `release-fix/<task>`
# from release-fixer, `release/<task>` from release-candidate, and
# `fixture/<task>` from scripts/release-fixture.sh. The reviewer only ever
# fixes findings on the existing branch and never opens a second pull request
# for the same one, so the newest pull request whose head starts with that
# prefix is always the one it reviewed.
#
# --optional: a repair round that finds nothing left to fix opens no pull
# request at all. With no PR to wait on and --optional set, exit 0 and let the
# pipeline continue; without it, that is a failure, since await-release and
# await-fixture are only ever reached once release-candidate or
# scripts/release-fixture.sh has already opened one.
set -euo pipefail

say() { printf 'release-await-merge: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

optional=false
if [ "${1:-}" = "--optional" ]; then
  optional=true
  shift
fi
prefix="${1:-}"
[ -n "$prefix" ] || die "usage: release-await-merge.sh [--optional] <branch-prefix>"

repo="$(git remote get-url origin)"

# gh lists newest-created first and pull request numbers only rise, so
# sorting by number descending and taking the first prefix match is the
# newest pull request on that prefix, open or already resolved.
pr_json="$(gh pr list --repo "$repo" --state all --limit 200 \
  --json number,headRefName,url,state \
  --jq "[.[] | select(.headRefName | startswith(\"$prefix\"))] | sort_by(-.number) | .[0] // empty")"

if [ -z "$pr_json" ]; then
  if $optional; then
    say "no pull request with head branch starting \"$prefix\" — nothing to wait for"
    exit 0
  fi
  die "no pull request with head branch starting \"$prefix\" to wait for"
fi

number="$(printf '%s' "$pr_json" | jq -r .number)"
url="$(printf '%s' "$pr_json" | jq -r .url)"
say "newest PR on $prefix: #$number"
say "waiting for you to merge $url"

while true; do
  # A transient `gh` or network error is not the same as the PR being closed
  # unmerged: without this, one flaky poll in a run of up to 360 over 6h
  # would exit non-zero, park the task on `blocked`, and hand a merge that
  # already happened — or is still coming — back to a person to resume.
  if ! read -r state merge_sha < <(gh pr view "$number" --repo "$repo" \
      --json state,mergeCommit --jq '"\(.state) \(.mergeCommit.oid // "-")"'); then
    say "$(date +%H:%M) could not read #$number's status — retrying"
    sleep 60
    continue
  fi
  case "$state" in
    MERGED)
      say "$(date +%H:%M) merged as ${merge_sha:0:7}"
      say "#$number merged — passing it on"
      exit 0
      ;;
    CLOSED)
      die "#$number was closed without merging"
      ;;
    *)
      say "$(date +%H:%M) $(printf '%s' "$state" | tr '[:upper:]' '[:lower:]')"
      sleep 60
      ;;
  esac
done
