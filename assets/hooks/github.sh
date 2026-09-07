#!/bin/sh
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on queued, blocked, paused and
# done.
#
# Every command here was run against a real repository with gh 2.97.0.
# `gh` has no sub-issue command, so the parent link goes through the REST
# endpoint, which wants the issue's integer id — not the node id that
# `gh issue view --json id` returns.

repo=$SPOOLWAY_PROJECT_KEY                # `[issue_tracking] project_key`

# Hangs `$2` (an issue this hook just created — the URL `gh issue create`
# printed) as a GitHub sub-issue under `$1`, doing nothing at all unless `$1`
# ends in `/<repo>/issues/<n>` for this same `$repo`. `$1` is `SPOOLWAY_SOURCE`
# most of the time, and spoolway never parses that field — most of the time
# it names a plan page's path, not an issue, and this is what lets it fall
# straight through untouched. Matched by the tail of the URL rather than a
# fixed `github.com` host, so this reads a GitHub Enterprise Server issue the
# same way. Reuses the `sub_issues` REST call the epic and ticket already use
# each other for, above: a second call from one already proven, not a new
# capability.
hang_under() {
  case "$1" in
    *"/$repo/issues/"[0-9]*) ;;
    *) return 0 ;;
  esac
  [ -z "$2" ] && return 0
  parent=${1##*/}
  gh api -X POST "repos/$repo/issues/$parent/sub_issues" \
    -F sub_issue_id="$(gh api "repos/$repo/issues/${2##*/}" -q .id)"
}

if [ "$SPOOLWAY_EVENT" = fetch ]; then
  gh issue view "$SPOOLWAY_REF" -R "$repo" \
    --json number,url,title,state,labels,body,comments \
    --jq '{ref: (.number | tostring), url, title, state: (.state | ascii_downcase),
           labels: [.labels[].name], body,
           comments: [.comments[] | {author: .author.login, body}]}' \
    > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT" = open ]; then
  epic=$SPOOLWAY_EPIC                       # set when the group already names one
  if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
    epic=$(gh issue create -R "$repo" -t "$SPOOLWAY_GROUP" \
                           -F "$SPOOLWAY_EPIC_BODY")
    hang_under "$SPOOLWAY_SOURCE" "$epic"
  fi
  ticket=$(gh issue create -R "$repo" -t "$SPOOLWAY_TITLE" \
                           -F "$SPOOLWAY_TICKET_BODY")
  if [ -n "$epic" ]; then                    # gh has no sub-issue command
    gh api -X POST "repos/$repo/issues/${epic##*/}/sub_issues" \
      -F sub_issue_id="$(gh api "repos/$repo/issues/${ticket##*/}" -q .id)"
  else
    hang_under "$SPOOLWAY_SOURCE" "$ticket"  # a group of one has no epic to hang under
  fi
  # The short handle spoolway puts in generated names, and the issue's web
  # address kept on the task for later use — spoolway stores and validates
  # `url=` but shows it nowhere yet. `$epic`/`$ticket` are already URLs here:
  # take the issue number off the end, with a `gh-` prefix so the slug starts
  # with a letter the way every spoolway id does. The epic keys a group, the
  # ticket keys a group of one that never opened an epic.
  key=${epic:-$ticket}
  { echo "epic=$epic"; echo "ticket=$ticket"
    echo "slug=gh-${key##*/}"; echo "url=$key"; } > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT$SPOOLWAY_GROUP_LAST" = done1 ] && [ -n "$SPOOLWAY_EPIC" ]; then
  gh issue close "$SPOOLWAY_EPIC" -c "spoolway: every task here is done."
fi

case "$SPOOLWAY_EVENT" in blocked|paused) ;; *) exit 0 ;; esac

{
  echo "**spoolway** — \`$SPOOLWAY_TASK\` is **$SPOOLWAY_EVENT** at \`$SPOOLWAY_FROM\`"
  echo
  echo '<details><summary>Task file</summary>'
  echo
  echo '```markdown'
  head -c 50000 "$SPOOLWAY_TASK_FILE"   # room under the comment body cap
  echo '```'
  echo '</details>'
} | gh issue comment "$SPOOLWAY_TICKET" --body-file -
