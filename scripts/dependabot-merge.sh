#!/usr/bin/env bash
# Carry out the `review` step's verdicts on open Dependabot pull requests:
# post each verdict as a comment, and turn on auto-merge for the approved
# ones. Run by the `merge` step of the dependabot pipeline, from the review
# worktree. A person can run it by hand from any checkout holding a verdicts
# file.
#
# A command step, not an agent, for the same reason as release-ship.sh:
# Claude Code's auto mode refuses a merge a lane makes.
#
# Nothing here merges. `gh pr merge --auto` hands the pull request to GitHub,
# which merges once the required checks (`verify / test`, `verify / audit`)
# pass. Before that, every approval is checked again here, so an approval
# these rules refuse is never acted on, whatever the review concluded:
#   - the pull request is open, Dependabot's, and still at the reviewed head
#   - every commit on it is Dependabot's
#   - it changes only Cargo.toml, Cargo.lock and `uses:` lines in workflows
#   - every action it pins to a SHA is the commit its `# <ref>` comment names
#   - no check on it has failed
#
# Every verdict carried out, and every approval refused, leaves a comment
# carrying `<!-- dependency-reviewer <head-sha> <verdict> -->`.
# scripts/dependabot-pending.sh reads that marker, so the pull request is not
# reviewed again until its head moves. Approvals are deferred instead, with
# no comment, when main's latest completed `ci` run failed or five were
# already handed to auto-merge this run; the next run reviews them again.
#
# Input, relative to the current directory:
#   tmp/dependabot/verdicts.tsv   `<pr>\t<head-sha>\t<merge|hold|fix>` per line
#   tmp/dependabot/<pr>.md        the comment posted on that pull request
#
# Exit 0: every verdict was carried out, refused with a comment, or deferred
# by the cap. Exit 1: main's ci is red, or something could not be carried
# out, named on stderr; every other verdict was still carried out.
set -euo pipefail

say() { printf 'dependabot-merge: %s\n' "$*" >&2; }

dir=tmp/dependabot
verdicts="$dir/verdicts.tsv"
max_merges=5
status=0
merges=0

[ -f "$verdicts" ] || { say "no $verdicts: nothing was reviewed"; exit 1; }

main_ci="$(gh run list --workflow ci.yml --branch main --status completed --limit 1 \
  --json conclusion --jq '.[0].conclusion // empty')"
main_red=0
if [ "$main_ci" = failure ]; then
  say "main's latest ci run failed: every approval is deferred"
  main_red=1
fi

# Every `<owner>/<repo>[/path]@<40-hex> # <ref>` this diff adds must name the
# commit <ref> points to upstream. A tag ref (`@v5`) is only accepted for the
# first-party `actions/` owner; third-party actions stay pinned to SHAs.
check_action_pins() { # <pr> — prints a reason and returns 1 on the first bad pin
  local line spec ref comment owner repo want
  while IFS= read -r line; do
    spec="$(sed -E 's/^\+[[:space:]]*(-[[:space:]]*)?uses:[[:space:]]*//; s/[[:space:]]*#.*$//' <<<"$line")"
    ref="${spec##*@}"
    owner="${spec%%/*}"
    repo="$(cut -d/ -f2 <<<"${spec%@*}")"
    case "$spec" in ./*) continue ;; esac
    if [[ "$ref" =~ ^[0-9a-f]{40}$ ]]; then
      comment="$(sed -nE 's/.*#[[:space:]]*([^[:space:]]+).*/\1/p' <<<"$line")"
      [ -n "$comment" ] || { echo "$spec is pinned with no # <ref> comment"; return 1; }
      want="$(gh api "repos/$owner/$repo/commits/$comment" --jq .sha 2>/dev/null)" \
        || { echo "$owner/$repo has no ref $comment"; return 1; }
      [ "$want" = "$ref" ] || { echo "$spec: $comment points to $want upstream"; return 1; }
    elif [ "$owner" != actions ]; then
      echo "$spec is a third-party action not pinned to a SHA"
      return 1
    fi
  done < <(gh pr diff "$1" | grep -E '^\+[[:space:]]*(-[[:space:]]*)?uses:' || true)
}

approve() { # <pr> <head-sha> — returns 1 with a reason on stdout when refused
  local pr="$1" head="$2" view bad changed reason
  view="$(gh pr view "$pr" --json author,state,headRefOid,commits,files,statusCheckRollup)"
  [ "$(jq -r .author.login <<<"$view")" = app/dependabot ] || { echo "not opened by Dependabot"; return 1; }
  [ "$(jq -r .state <<<"$view")" = OPEN ] || { echo "not open"; return 1; }
  [ "$(jq -r .headRefOid <<<"$view")" = "$head" ] || { echo "head moved off the reviewed $head"; return 1; }

  bad="$(jq -r '[.commits[].authors[].login | select(. != "dependabot[bot]")] | unique | join(", ")' <<<"$view")"
  [ -z "$bad" ] || { echo "has commits by $bad"; return 1; }

  bad="$(jq -r '[.files[].path
    | select(. != "Cargo.toml" and . != "Cargo.lock" and (test("^\\.github/workflows/[^/]+\\.ya?ml$") | not))]
    | join(", ")' <<<"$view")"
  [ -z "$bad" ] || { echo "changes $bad"; return 1; }

  # In workflow files, every changed line must be a `uses:` line.
  changed="$(gh pr diff "$pr" | awk '
    /^diff --git / { wf = ($3 ~ /^a\/\.github\/workflows\//) }
    /^(\+\+\+|---) / { next }
    wf && /^[+-]/ && $0 !~ /^[+-][[:space:]]*(-[[:space:]]*)?uses:/ { print; exit }')"
  [ -z "$changed" ] || { echo "changes a workflow line that is not a uses: line: $changed"; return 1; }

  reason="$(check_action_pins "$pr")" || { echo "$reason"; return 1; }

  bad="$(jq -r '[.statusCheckRollup[]
    | select((.conclusion // .state) as $c | ["FAILURE","ERROR","CANCELLED","TIMED_OUT","ACTION_REQUIRED"] | index($c))
    | (.name // .context)] | join(", ")' <<<"$view")"
  [ -z "$bad" ] || { echo "failed checks: $bad"; return 1; }
}

# Posted once per reviewed head and verdict, so a re-run never repeats one.
comment() { # <pr> <head-sha> <verdict> [extra paragraph]
  local marker="<!-- dependency-reviewer $2 $3 -->" body="$dir/$1.md" posted
  [ -f "$body" ] || { say "#$1: no $body to post"; return 1; }
  posted="$(gh pr view "$1" --json comments --jq '.comments[].body')" || return 1
  if grep -qF "$marker" <<<"$posted"; then
    return 0
  fi
  { cat "$body"; [ -z "${4:-}" ] || printf '\n%s\n' "$4"; printf '\n%s\n' "$marker"; } \
    | gh pr comment "$1" --body-file - >/dev/null
}

# Read on fd 3, so no `gh` call inside the loop can swallow the verdicts.
while IFS=$'\t' read -r pr head verdict <&3; do
  [ -n "$pr" ] || continue
  case "$verdict" in
    merge)
      if [ "$main_red" = 1 ]; then
        say "#$pr deferred: main's latest ci run failed"
        status=1
      elif [ "$merges" -ge "$max_merges" ]; then
        say "#$pr deferred: $max_merges already handed to auto-merge this run"
      elif reason="$(approve "$pr" "$head")"; then
        # Merge first: a comment left by a failed merge would stop the next
        # run from reviewing it again.
        if gh pr merge "$pr" --auto --squash --match-head-commit "$head"; then
          merges=$((merges + 1))
          say "#$pr: auto-merge on, waiting for the required checks"
          comment "$pr" "$head" merge || status=1
        else
          say "#$pr: gh pr merge --auto failed"
          status=1
        fi
      else
        say "#$pr refused: $reason"
        comment "$pr" "$head" refused \
          "**Not merged.** The reviewer approved this, but the merge rules refused it: $reason" \
          || status=1
      fi
      ;;
    hold | fix)
      comment "$pr" "$head" "$verdict" || status=1
      say "#$pr: $verdict"
      ;;
    *)
      say "#$pr: unknown verdict '$verdict'"
      status=1
      ;;
  esac
done 3<"$verdicts"

say "$merges pull request(s) handed to auto-merge"
exit "$status"
