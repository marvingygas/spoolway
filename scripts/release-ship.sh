#!/usr/bin/env bash
# Ship the release pull request the `prepare` step opened: wait for its
# checks, rehearse the release workflow on it, merge it, tag the release
# commit, watch the tag's own workflow publish it, and verify what npm and
# GitHub now serve. Run by the `ship` step of the release pipeline, from the
# release worktree, whose Cargo.toml carries the version being released. A
# person can run it by hand from the same place.
#
# A command step, not an agent, because Claude Code's auto mode refused both
# the merge ("Merge Without Review") and the tag push ("Create Public
# Surface") when a lane did them. A command step is never shown to auto mode.
#
# The rehearsal runs on the pull request's head before the merge, so a red
# build or pack is found while it is still a branch, not an untagged release
# commit on main. `main` is protected with strict status checks, so a branch
# that fell behind is rebased onto main and checked and rehearsed again.
#
# Idempotent: a re-run after the merge skips to tagging, and a re-run after
# the tag push skips to watching the tag's run and verifying.
#
# Runs are judged by job conclusions read back with `gh`, never by a
# `gh run watch` exit code, which is zero for a run with a failed job.
set -euo pipefail

say() { printf 'release-ship: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(git remote get-url origin)"
workflow=release.yml

version="$(awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}' Cargo.toml)"
[ -n "$version" ] || die "could not read the package version from Cargo.toml"
tag="v$version"
branch="release/$tag"

wait_for_run() { # <event> <ref> <sha> [since-iso8601] -> prints the run's id
  local event="$1" ref="$2" sha="$3" since="${4:-}" filter id=""
  filter=".headSha == \"$sha\""
  [ -n "$since" ] && filter="$filter and .createdAt >= \"$since\""
  for _ in $(seq 1 60); do
    id="$(gh run list --repo "$repo" --workflow "$workflow" --branch "$ref" \
      --event "$event" --limit 20 --json databaseId,headSha,createdAt \
      --jq "[.[] | select($filter)] | sort_by(.databaseId) | last | .databaseId // empty")"
    [ -n "$id" ] && { printf '%s\n' "$id"; return 0; }
    sleep 5
  done
  return 1
}

watch_run() { # <run-id> — blocks until the run has completed
  while [ "$(gh run view "$1" --repo "$repo" --json status --jq .status)" != completed ]; do
    sleep 30
  done
}

require_green() { # <run-id> [job allowed to be skipped]
  local id="$1" skip="${2:-}" bad="" jobs name concl
  jobs="$(gh run view "$id" --repo "$repo" --json jobs --jq '.jobs[] | "\(.name)=\(.conclusion)"')" \
    || die "could not read the jobs of run $id"
  [ -n "$jobs" ] || die "run $id reported no jobs at all"
  while IFS='=' read -r name concl; do
    if [ "$name" = "$skip" ]; then
      [ "$concl" = skipped ] || bad="$bad $name=$concl"
    else
      [ "$concl" = success ] || bad="$bad $name=$concl"
    fi
  done <<<"$jobs"
  [ -z "$bad" ] || die "run $id has jobs that are not green:$bad"
}

wait_checks() { # <pr> <head-sha> — returns once every check on that head is green
  local pr="$1" head="$2" rows pending bad name concl
  while true; do
    [ "$(gh pr view "$pr" --repo "$repo" --json headRefOid --jq .headRefOid)" = "$head" ] \
      || die "pull request #$pr moved off $head while its checks ran — run this step again"
    rows="$(gh pr view "$pr" --repo "$repo" --json statusCheckRollup --jq '.statusCheckRollup[]
      | "\(.name // .context)=\(if .status then (if .status == "COMPLETED" then .conclusion else "PENDING" end) else .state end)"')"
    # The two contexts branch protection requires. Until both have reported,
    # the checks have not all started yet.
    if grep -q '^verify / test=' <<<"$rows" && grep -q '^verify / audit=' <<<"$rows"; then
      pending="" bad=""
      while IFS='=' read -r name concl; do
        case "$concl" in
          SUCCESS | SKIPPED | NEUTRAL) ;;
          PENDING | EXPECTED | QUEUED | IN_PROGRESS | "") pending=1 ;;
          *) bad="$bad $name=$concl" ;;
        esac
      done <<<"$rows"
      [ -z "$bad" ] || die "pull request #$pr has red checks:$bad — fix them on $branch, then run this step again"
      [ -n "$pending" ] || { say "pull request #$pr: every check is green on $head"; return 0; }
    fi
    sleep 30
  done
}

rehearse() { # <head-sha> — the release workflow with publication off
  local head="$1" since id
  since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  gh workflow run "$workflow" --repo "$repo" --ref "$branch" -f publish=false
  id="$(wait_for_run workflow_dispatch "$branch" "$head" "$since")" \
    || die "could not find the dispatched rehearsal run on $head"
  watch_run "$id"
  require_green "$id" assets
  say "rehearsal run $id on $head is green, with assets skipped by design"
}

land_pr() { # <pr>
  local pr="$1" head attempt
  for attempt in 1 2 3; do
    if [ "$(gh pr view "$pr" --repo "$repo" --json mergeStateStatus --jq .mergeStateStatus)" = BEHIND ]; then
      say "pull request #$pr is behind main, so it is rebased onto it"
      gh pr update-branch "$pr" --repo "$repo" --rebase
      sleep 15
    fi
    head="$(gh pr view "$pr" --repo "$repo" --json headRefOid --jq .headRefOid)"
    wait_checks "$pr" "$head"
    rehearse "$head"
    # Rebase, so the release commit lands as its own commit with its subject
    # intact. `--match-head-commit` refuses the merge if the branch moved
    # after the rehearsal.
    if gh pr merge "$pr" --repo "$repo" --rebase --match-head-commit "$head"; then
      say "merged pull request #$pr"
      return 0
    fi
    say "merge attempt $attempt refused, most likely because main moved — trying again"
  done
  die "pull request #$pr could not be merged after 3 attempts"
}

release_commit() { # prints the newest commit on origin/main whose subject names this release
  local sha rest
  while IFS=' ' read -r sha rest; do
    case "$rest" in
      "chore(release): $tag" | "chore(release): $tag (#"*")") printf '%s\n' "$sha"; return 0 ;;
    esac
  done < <(git log --format='%H %s' -n 300 origin/main)
}

git fetch --quiet --tags --force origin

# Asked of origin, never of a local ref: a local tag from an interrupted run
# may never have reached origin.
remote_tag_sha="$(git ls-remote --tags origin "refs/tags/$tag" | awk '{print $1; exit}')"

if [ -n "$remote_tag_sha" ]; then
  release_sha="$remote_tag_sha"
  say "$tag is already on origin at $release_sha"
else
  release_sha="$(release_commit)"
  if [ -z "$release_sha" ]; then
    pr="$(gh pr list --repo "$repo" --head "$branch" --state open --json number --jq '.[0].number // empty')"
    [ -n "$pr" ] || die "no open pull request from $branch and no 'chore(release): $tag' commit on origin/main"
    land_pr "$pr"
    git fetch --quiet origin
    release_sha="$(release_commit)"
    [ -n "$release_sha" ] || die "pull request #$pr merged, but no 'chore(release): $tag' commit is on origin/main"
  fi

  local_tag_sha="$(git rev-parse --verify --quiet "refs/tags/$tag^{commit}" 2>/dev/null || true)"
  if [ -z "$local_tag_sha" ]; then
    git tag "$tag" "$release_sha"
  elif [ "$local_tag_sha" != "$release_sha" ]; then
    die "local tag $tag is at $local_tag_sha, not the release commit $release_sha — delete it and run this step again"
  fi
  git push origin "refs/tags/$tag"
  say "pushed $tag at $release_sha"
fi

tag_run="$(wait_for_run push "$tag" "$release_sha")" \
  || die "could not find the tag's own workflow run on $release_sha"
watch_run "$tag_run"
require_green "$tag_run"
say "tag run $tag_run on $tag is green"

"$here/release-verify.sh" "$version"
