#!/usr/bin/env bash
# Say whether the dependabot pipeline's review step has anything to do, so
# an agent is only started when a pull request needs one. Run by the `check`
# step; a person can run it by hand from any checkout.
#
# A pull request needs review when it is open, Dependabot's, and carries no
# `<!-- dependency-reviewer <head-sha> ... -->` comment for its current head.
# scripts/dependabot-merge.sh posts one with every verdict it carries out,
# so a held pull request is looked at again only once Dependabot pushes to
# it. One already set to auto-merge (by dependabot-auto-merge.yml) with no
# failed check is left to GitHub.
#
# Exit 0: at least one pull request needs review, or the lookup failed — a
# failed lookup must start the review, never skip it. Exit 1: none does.
set -uo pipefail

say() { printf 'dependabot-pending: %s\n' "$*" >&2; }

prs="$(gh pr list --author app/dependabot --state open --limit 100 \
  --json number,headRefOid,autoMergeRequest,statusCheckRollup,comments)" \
  || { say "could not list pull requests: reviewing anyway"; exit 0; }

pending="$(jq -r '.[]
  | .headRefOid as $head
  | select([.comments[].body | contains("<!-- dependency-reviewer \($head) ")] | any | not)
  | select(.autoMergeRequest == null
      or ([.statusCheckRollup[] | (.conclusion // .state)
           | select(IN("FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED"))] | length > 0))
  | "#\(.number)"' <<<"$prs")" \
  || { say "could not read the pull request list: reviewing anyway"; exit 0; }

if [ -z "$pending" ]; then
  say "no Dependabot pull request needs review"
  exit 1
fi
say "to review: $(tr '\n' ' ' <<<"$pending")"
