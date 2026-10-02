#!/usr/bin/env bash
# Run by hand after a task's pull request merges, on a machine where `acli`
# is already logged in: `.github/scripts/close-jira.sh <pr number>`. Never
# from GitHub Actions — this project holds no Jira API token, and a runner
# has no other way to log `acli` in.
#
# Takes the merged pull request's number as $1 and sweeps the Jira Story
# its branch names: every Sub-task whose own "Ready for review in <PR>"
# comment names a PR that has since merged is closed, and the Story closes
# behind it once none of its Sub-tasks are left open. A run always sweeps
# the whole Story rather than just the PR it was given (see the plan this
# shipped with, 2026-10-02-tracker-hook-parity.html#d-sweep), so after a
# stack merge one run with any of the stack's pull requests closes every
# merged Sub-task and the Story with it.
#
# Needs `acli` (logged in against the Jira site, the same login
# `assets/hooks/jira.sh` uses), `gh` (logged in against this repository)
# and `jq`. Run from a checkout of this repository: `gh pr view` below is
# only ever given a bare PR number for the triggering pull request, which
# `gh` resolves against the checkout's own remote, the same assumption
# `assets/hooks/jira.sh`'s own `done` event already makes.
#
# This project's own Jira project key — the one piece of this script that
# is this site's own, same as the status name below. A project whose key
# is not KAN edits this one line.
project=KAN
#
# The status a closed Sub-task or Story moves to, named once here so a
# project whose workflow spells it differently edits one line instead of
# hunting through the script below for it — the same trade-off
# `assets/hooks/jira.sh` already makes for Draft/In Progress/Review. A
# transition that cannot reach this status fails loudly below, naming it,
# rather than leaving an item stuck silently in whatever status it had.
#
# This repository's own KAN project turned out to need `Resolved`, not the
# `Done` most team-managed Jira projects ship with — live proof against
# spoolway.atlassian.net found no transition reaches a status named
# "Done" at all, but one named "Resolved" every time, the same kind of
# per-site mismatch `assets/hooks/jira.sh`'s own header already warns
# about for its three status names.
status_done=Resolved

set -eE
trap 'close_jira_trace "$LINENO" "$BASH_COMMAND"' ERR

# Same shape as `assets/hooks/jira.sh`'s own `hook_trace`: prints the
# command, its line and the call chain that failed, then exits with that
# command's own code — see that file's header for why each piece of this
# is here (the subshell check, $BASH_COMMAND being unexpanded source text).
close_jira_trace() {
  local code=$? line cmd callers i
  line=$1
  cmd=$2
  if [ "$BASH_SUBSHELL" -gt 0 ]; then
    exit "$code"
  fi
  printf '%s: command failed (exit %d)\n' "$(basename "$0")" "$code" >&2
  printf '  at line %s: %s\n' "$line" "$cmd" >&2
  callers=
  i=1
  while [ "$i" -lt "${#FUNCNAME[@]}" ]; do
    callers="${callers:+$callers -> }${FUNCNAME[$i]}"
    i=$((i + 1))
  done
  printf '  called from %s\n' "${callers:-main}" >&2
  exit "$code"
}

# `Merged <PR URL>` as ADF, the full URL as its own link text, the way
# the plan's #m-closed mockup draws it. This is the fixed one-line shape
# every close in this script writes, so it never needs the full
# Markdown-to-ADF filter `assets/hooks/jira.sh` carries for task bodies.
merged_adf() {
  jq -n --arg url "$1" '
    {type:"doc", version:1, content:[{type:"paragraph", content:[
      {type:"text", text:"Merged "},
      {type:"text", text:$url, marks:[{type:"link", attrs:{href:$url}}]}
    ]}]}'
}

# The `owner/repo#n` reference named by the most recent "Ready for review
# in …" comment on $1, or nothing when that item was never given one — a
# Sub-task opened before this sweep existed has no such comment, and the
# caller leaves it open rather than guess which pull request it means.
# Comments come back oldest first, so the last match in the list is the
# most recent.
#
# This reads the comment's own plain text, never the link mark's href:
# live proof against spoolway.atlassian.net found that `acli jira workitem
# comment list --json` already flattens a comment's ADF body down to text
# before it ever reaches this script — the same body `assets/hooks/jira.sh`
# writes as `Ready for review in [owner/repo#n](url)` comes back here as
# the bare words `Ready for review in owner/repo#n`, the href gone
# entirely. The short reference is still enough: `gh pr view` below takes
# it apart itself.
review_pr_ref() {
  local comments
  comments=$(acli jira workitem comment list --key "$1" --json 2>/dev/null) || true
  [ -n "$comments" ] || comments='{"comments":[]}'
  printf '%s' "$comments" | jq -r '
    def text: if type == "string" then . else "" end;
    [.comments[]? | .body | text | select(startswith("Ready for review in"))]
    | if length == 0 then "" else (.[-1] | capture("(?<ref>[^\\s/]+/[^\\s/]+#[0-9]+)").ref) end
  ' 2>/dev/null
}

# `state` and `url` for the pull request $1 names, as `owner/repo#n` —
# `gh pr view` needs the owner/repo split off into `-R` and the bare number
# as its own argument, not the one `#`-joined string this script and the
# Jira comment both carry it as elsewhere.
pr_view() {
  local repo=${1%#*} number=${1##*#}
  gh pr view "$number" -R "$repo" --json state,url
}

# Assigns $1 to $2 (an accountId), comments "Merged $3" on it and moves it
# to $status_done — the three steps every closed Sub-task and Story get,
# in the one order the mockup draws them. Every variable here is `local`:
# `pr_url` is also the triggering pull request's URL at the top level, and
# a Sub-task closed first would otherwise leave its own URL there for the
# Story's "Merged" comment (live finding: KAN-50 got #616, not #620).
close_item() {
  local key=$1 reporter=$2 pr_url=$3 comment_adf
  acli jira workitem assign --key "$key" --assignee "$reporter" --yes
  comment_adf=$(mktemp "${TMPDIR:-/tmp}/close-jira-adf.XXXXXX")
  merged_adf "$pr_url" > "$comment_adf"
  acli jira workitem comment create --key "$key" --body-file "$comment_adf"
  rm -f "$comment_adf"
  acli jira workitem transition --key "$key" --status "$status_done" --yes || {
    echo "close-jira.sh: could not transition $key to \"$status_done\" — does this site's workflow use that name?" >&2
    exit 1
  }
}

pr_number=${1:?"usage: close-jira.sh <pull request number>"}

pr_url=$(gh pr view "$pr_number" --json url --jq .url)
branch=$(gh pr view "$pr_number" --json headRefName --jq .headRefName)

# `task/kan-40-board-reads-changed-only` names Story KAN-40; a branch with
# no such prefix — including this repo's own GitHub-flavoured
# `task/gh-123-…` branches, which this script never touches — carries no
# Jira key at all, so this logs and exits rather than guessing one.
story=$(printf '%s' "$branch" | sed -nE "s#^task/(${project,,}-[0-9]+)-.*#\\1#p" | tr '[:lower:]' '[:upper:]')
if [ -z "$story" ]; then
  echo "close-jira.sh: branch \"$branch\" names no $project Story — nothing to close"
  exit 0
fi

# Every Sub-task of the Story not already $status_done — read once,
# client-side filtered below rather than as `status != "$status_done"` in
# the JQL itself: Jira Cloud rejects a JQL status value that has never yet
# been assigned to any issue on the whole site (live finding against
# spoolway.atlassian.net — "the value 'done' does not exist for the field
# 'status'" — even though it is a perfectly real status no item had
# reached yet), and the first-ever run of this script is exactly that case.
subtasks=$(acli jira workitem search --jql "parent = $story" --fields "key,status,reporter" --json)
still_open=0
# `< <(...)`, not `... | while`: a `while` on the *reading* end of an
# ordinary pipe runs in its own subshell, and the ERR trap above already
# treats a subshell's own failure as the quiet, already-reported half of a
# double firing (see `close_jira_trace`'s own comment) — exactly wrong
# here, where this loop is the only place the failure would ever be
# reported at all. Process substitution keeps the loop body in this same
# shell, so a failed `close_item` traces loudly, and `still_open` set
# below survives to the Story check that follows.
while IFS= read -r item; do
  key=$(printf '%s' "$item" | jq -r '.key')
  status=$(printf '%s' "$item" | jq -r '.fields.status.name')
  if [ "$status" = "$status_done" ]; then
    continue
  fi

  ref=$(review_pr_ref "$key")
  if [ -z "$ref" ]; then
    echo "close-jira.sh: $key has no \"Ready for review in\" comment — leaving it open"
    still_open=$((still_open + 1))
    continue
  fi
  # A failed lookup is not an unmerged pull request: it is logged as a gh
  # problem, so the person running this can tell the two apart.
  if ! view=$(pr_view "$ref" 2>/dev/null); then
    echo "close-jira.sh: could not read $key's pull request ($ref) from gh — check the gh login and that the repository exists; leaving it open" >&2
    still_open=$((still_open + 1))
    continue
  fi
  state=$(printf '%s' "$view" | jq -r '.state // empty')
  if [ "$state" != "MERGED" ]; then
    echo "close-jira.sh: $key's pull request ($ref) has not merged — leaving it open"
    still_open=$((still_open + 1))
    continue
  fi

  review_url=$(printf '%s' "$view" | jq -r '.url')
  reporter=$(printf '%s' "$item" | jq -r '.fields.reporter.accountId // empty')
  close_item "$key" "$reporter" "$review_url"
  echo "close-jira.sh: closed $key ($review_url merged)"
done < <(printf '%s' "$subtasks" | jq -c '.[]')

if [ "$still_open" -gt 0 ]; then
  echo "close-jira.sh: $still_open Sub-task(s) of $story still open — Story not closed"
  exit 0
fi

story_fields=$(acli jira workitem view "$story" --fields "status,reporter" --json)
story_status=$(printf '%s' "$story_fields" | jq -r '.fields.status.name')
if [ "$story_status" = "$status_done" ]; then
  echo "close-jira.sh: $story is already $status_done — nothing to do"
  exit 0
fi

story_reporter=$(printf '%s' "$story_fields" | jq -r '.fields.reporter.accountId // empty')
close_item "$story" "$story_reporter" "$pr_url"
echo "close-jira.sh: closed $story ($pr_url merged, no Sub-task left open)"
