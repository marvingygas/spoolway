#!/usr/bin/env bash
# Say whether the security pipeline's fix step has anything to do, so an
# agent is only started when a code-scanning alert needs one. Run by the
# `check` step; a person can run it by hand from any checkout.
#
# An alert needs a fix when it is open and no open pull request already
# carries a `Code-Scanning-Alert: <number>` trailer for it. The fix step
# writes that trailer on its commits, so an alert whose fix is waiting for
# review is not fixed a second time. Of the alerts left, the most severe
# one is picked, the oldest first on a tie, and written to
# `tmp/code-scanning/alert.json` for the fix step to read.
#
# Exit 0: an alert needs a fix, or the lookup failed — a failed lookup must
# start the fix step, which runs this again itself, never skip it. Exit 1:
# no alert needs one.
set -uo pipefail

say() { printf 'code-scanning-pending: %s\n' "$*" >&2; }

out=tmp/code-scanning/alert.json
rm -f "$out"

alerts="$(gh api --paginate 'repos/{owner}/{repo}/code-scanning/alerts?state=open&per_page=100' \
  | jq -s 'add // []')" \
  || { say "could not list code-scanning alerts: fixing anyway"; exit 0; }

# One REST call per pull request: asking GraphQL for every open pull
# request's commits at once is refused as too large a query.
prs="$(gh pr list --state open --limit 100 --json number --jq '.[].number')" \
  || { say "could not list open pull requests: fixing anyway"; exit 0; }
messages=""
for pr in $prs; do
  messages+="$(gh api --paginate "repos/{owner}/{repo}/pulls/$pr/commits?per_page=100" \
    --jq '.[].commit.message')"$'\n' \
    || { say "could not read pull request #$pr's commits: fixing anyway"; exit 0; }
done
taken="$(jq -Rs '[scan("(?m)^Code-Scanning-Alert: *#?([0-9]+)") | .[0] | tonumber] | unique' \
  <<<"$messages")" \
  || { say "could not read the commit messages: fixing anyway"; exit 0; }

alert="$(jq --argjson taken "$taken" '
  def rank: {"critical": 0, "high": 1, "medium": 2, "low": 3,
             "error": 1, "warning": 2, "note": 3}[.] // 4;
  [.[] | select(.number as $n | $taken | index($n) | not)]
  | sort_by((.rule.security_severity_level // .rule.severity | rank), .number)
  | first // empty
  | {number, html_url,
     rule: .rule.id, severity: (.rule.security_severity_level // .rule.severity),
     description: .rule.description, help: .rule.help,
     tool: .tool.name,
     path: .most_recent_instance.location.path,
     start_line: .most_recent_instance.location.start_line,
     end_line: .most_recent_instance.location.end_line,
     message: .most_recent_instance.message.text}' <<<"$alerts")" \
  || { say "could not read the alert list: fixing anyway"; exit 0; }

if [ -z "$alert" ]; then
  say "no open code-scanning alert needs a fix"
  exit 1
fi
mkdir -p "$(dirname "$out")"
printf '%s\n' "$alert" >"$out"
say "to fix: #$(jq -r .number <<<"$alert") $(jq -r .rule <<<"$alert") in $(jq -r .path <<<"$alert")"
