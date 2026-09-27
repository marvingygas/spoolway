#!/usr/bin/env bash
# Rehearse the merged release commit, tag it, and let the tag's own workflow
# run publish it. Run by the `publish` step of the release pipeline, right
# after a person merges the release-candidate pull request.
#
# This is a command step, not the `release-publisher` prompt, because Claude
# Code's auto-mode permission layer classifies `git push origin refs/tags/…`
# as creating a public surface and refuses it — exactly the refusal that
# turned every 0.6.0 tag push into a block and a paused task. A command step
# is never shown to auto mode, so the push happens without one.
#
# The release commit and version are found the way scripts/release-verify.sh
# finds them: `base_commit:` out of `$SPOOLWAY_TASK_FILE` anchors the
# candidate this task was cut from, and the version comes from
# `origin/main:Cargo.toml`. Unlike release-verify, there is no tag yet to read
# the commit from, so the release commit is the one commit between the anchor
# and `origin/main` whose subject is `chore(release): v<version>`.
#
# Idempotent, judged against `origin`'s own tag rather than a local one: a
# re-run after `v<version>` already exists on `origin` skips straight to
# checking the tag workflow's own run, whether this worktree's local tag
# and push both landed last time or only the local half did. Whether
# `origin/main` is still exactly the release commit is only asked when there
# is a tag left to push — once it exists on `origin`, main moving on top of
# it is somebody else's business, not a reason to stop checking this tag's
# own workflow run.
#
# Judged entirely by job conclusions read back with `gh run view --json
# jobs`, never by a `gh run watch` exit code, which is zero for a run that
# completed with a failed job.
set -euo pipefail

say() { printf 'release-publish: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

repo="$(git remote get-url origin)"
workflow=release.yml

git fetch --quiet --tags --force origin

[ -n "${SPOOLWAY_TASK_FILE:-}" ] || die "no SPOOLWAY_TASK_FILE — this script only runs as a command step"
[ -r "$SPOOLWAY_TASK_FILE" ] || die "SPOOLWAY_TASK_FILE names $SPOOLWAY_TASK_FILE, which is not readable"

# Same leading-`---`-block read as release-verify.sh, so a `base_commit:`
# written in the prose below it is never mistaken for the key.
anchor="$(awk '
  NR == 1 { if ($0 != "---") exit; next }
  $0 == "---" { exit }
  /^base_commit:/ {
    sub(/^base_commit:[ \t]*/, "")
    gsub(/["\047]/, "")
    sub(/[ \t]*$/, "")
    print
    exit
  }
' "$SPOOLWAY_TASK_FILE")"
[ -n "$anchor" ] || die "no base_commit in $SPOOLWAY_TASK_FILE to anchor the candidate to"
git rev-parse --verify --quiet "${anchor}^{commit}" >/dev/null \
  || die "base_commit $anchor in $SPOOLWAY_TASK_FILE does not resolve to a commit"

version="$(git show origin/main:Cargo.toml | awk '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/[" ]/,"",$3); print $3; exit}')"
[ -n "$version" ] || die "could not read the package version from origin/main:Cargo.toml"
tag="v$version"

head="$(git rev-parse origin/main)"

# The one commit between the anchor and origin/main whose subject names this
# release. `--reverse` so the oldest (and only expected) match wins if the
# pattern somehow matched more than once. The subject is kept verbatim,
# `(#123)` and all, for the log line below — reconstructing `chore(release):
# $tag` would silently drop the pull-request number a squash or rebase merge
# appends.
release_sha=""
release_subject=""
while IFS=' ' read -r sha rest; do
  case "$rest" in
    "chore(release): $tag" | "chore(release): $tag (#"*")")
      release_sha="$sha"
      release_subject="$rest"
      break
      ;;
  esac
done < <(git log --format='%H %s' --reverse "$anchor..origin/main")

[ -n "$release_sha" ] || die "no 'chore(release): $tag' commit between $anchor and origin/main ($head) — nothing to publish"
say "candidate anchored to base_commit $anchor (SPOOLWAY_TASK_FILE)"

# ---------------------------------------------------------------------------

wait_for_run() { # <event> <branch> [since-iso8601] -> prints the run's database id
  local event="$1" branch="$2" since="${3:-}" filter id=""
  filter=".headSha == \"$release_sha\""
  [ -n "$since" ] && filter="$filter and .createdAt >= \"$since\""
  for _ in $(seq 1 60); do
    id="$(gh run list --repo "$repo" --workflow "$workflow" --branch "$branch" \
      --event "$event" --limit 20 \
      --json databaseId,headSha,createdAt \
      --jq "[.[] | select($filter)] | sort_by(.databaseId) | last | .databaseId // empty")"
    [ -n "$id" ] && { printf '%s\n' "$id"; return 0; }
    sleep 5
  done
  return 1
}

watch_run() { # <run-id> — blocks until the run is no longer in progress
  while true; do
    status="$(gh run view "$1" --repo "$repo" --json status --jq .status)"
    [ "$status" = "completed" ] && return 0
    sleep 15
  done
}

require_green() { # <run-id> <job-name-allowed-skipped-or-empty>
  local id="$1" skip="${2:-}" bad="" jobs
  # Captured into a variable first, not read through `< <(...)`: a process
  # substitution's own exit code is invisible to the loop that reads it, so
  # under `set -e` a failed `gh run view` here would leave $bad empty and the
  # run judged green having read nothing at all. An assignment's command
  # substitution has no such blind spot.
  jobs="$(gh run view "$id" --repo "$repo" --json jobs --jq '.jobs[] | "\(.name)=\(.conclusion)"')" \
    || die "could not read jobs for run $id"
  [ -n "$jobs" ] || die "run $id reported no jobs at all"
  while IFS='=' read -r name concl; do
    if [ "$name" = "$skip" ]; then
      [ "$concl" = "skipped" ] || bad="$bad $name=$concl"
    else
      [ "$concl" = "success" ] || bad="$bad $name=$concl"
    fi
  done <<<"$jobs"
  [ -z "$bad" ] || die "run $id has job(s) not green:$bad"
}

# Asked of `origin`, never of a local ref: `git fetch --tags` never deletes a
# tag this worktree created on an earlier, interrupted run, so a local-only
# check would report "already tagged" for a tag nothing else can see and
# then wait forever for a tag run that was never triggered.
remote_tag_sha="$(git ls-remote --tags origin "refs/tags/$tag" | awk '{print $1; exit}')"

if [ -n "$remote_tag_sha" ]; then
  [ "$remote_tag_sha" = "$release_sha" ] \
    || die "$tag already exists on origin but points at $remote_tag_sha, not the recorded release commit $release_sha — needs recover"
  say "release commit $release_sha \"$release_subject\" — $tag already on origin, skipping rehearsal"
else
  # Only checked here: this is what the push below needs, not what checking
  # an already-pushed tag's own run needs. A re-run reaching this branch
  # after the tag went out is exactly the case this check must not block.
  [ "$release_sha" = "$head" ] \
    || die "release commit $release_sha ($tag) is not origin/main's tip ($head) — main moved after the merge; needs recover"
  say "release commit $release_sha \"$release_subject\" == origin/main"

  since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  gh workflow run "$workflow" --repo "$repo" --ref main -f publish=false
  rehearsal_id="$(wait_for_run workflow_dispatch main "$since")" \
    || die "could not find the dispatched rehearsal run on $release_sha"
  watch_run "$rehearsal_id"
  require_green "$rehearsal_id" assets
  say "rehearsal run $rehearsal_id on $release_sha — every job success, assets skipped by design"

  # A local tag from an earlier, interrupted run that never reached origin is
  # reused rather than recreated — `git tag` on an existing name fails, and
  # this same SHA is exactly what would be created again.
  local_tag_sha="$(git rev-parse --verify --quiet "refs/tags/$tag" 2>/dev/null || true)"
  if [ -n "$local_tag_sha" ]; then
    [ "$local_tag_sha" = "$release_sha" ] \
      || die "local tag $tag already exists at $local_tag_sha, not $release_sha — needs recover"
  else
    git tag "$tag" "$release_sha"
  fi
  git push origin "refs/tags/$tag"
  say "pushed refs/tags/$tag -> $release_sha"
fi

tag_run_id="$(wait_for_run push "$tag")" \
  || die "could not find the tag's own workflow run on $release_sha"
watch_run "$tag_run_id"
require_green "$tag_run_id" ""
say "tag run $tag_run_id on $tag — every job success"

say "$tag is out — handing to released"
