#!/bin/sh
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on queued, blocked, paused and
# done.
#
# Every command here was checked against gh 2.97.0. The hook treats GitHub as
# a mirror: issue failures are reported back to spoolway, while config decides
# whether they should pause delivery.

repo=$SPOOLWAY_PROJECT_KEY                # `[issue_tracking] project_key`

# Whether a source is an issue in this repository. A matching source becomes
# the parent of the group issue (or of the lone task issue for a group of one).
same_repo_issue() {
  case "$1" in
    *"/$repo/issues/"[0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

# GitHub issues only have open/closed as native states. This label means the
# task has entered spoolway; blocked and paused are comments instead of state
# labels because spoolway has no matching event when either condition clears.
mark_in_progress() {
  gh issue edit "$SPOOLWAY_TICKET" -R "$repo" \
    --add-label spoolway:in-progress
}

# `done` means spoolway handed the task to a pull request, not that the change
# merged. Leave closure to GitHub's merge event: mark the issue for review and
# put a machine-readable issue marker on the PR for the repository workflow.
hand_off_for_review() {
  pr=$(gh pr view "$SPOOLWAY_BRANCH" -R "$repo" --json url --jq .url) || exit $?
  [ -n "$pr" ] || {
    echo "github.sh: no pull request found for $SPOOLWAY_BRANCH" >&2
    exit 1
  }

  # Write the merge marker first: if a later cosmetic update fails under the
  # mirror's non-blocking `on_fail`, GitHub can still close the issue safely.
  gh pr comment "$pr" -R "$repo" --body \
    "**spoolway:** tracks $SPOOLWAY_TICKET

<!-- spoolway-issue: $SPOOLWAY_TICKET -->" || exit $?

  gh issue edit "$SPOOLWAY_TICKET" -R "$repo" \
    --remove-label spoolway:in-progress \
    --add-label spoolway:review || exit $?

  gh issue comment "$SPOOLWAY_TICKET" -R "$repo" --body \
    "**spoolway** — \`$SPOOLWAY_TASK\` is ready for review in $pr. GitHub will close this issue after the pull request merges."
}

comment_snapshot() {
  {
    echo "**spoolway** — \`$SPOOLWAY_TASK\` is **$SPOOLWAY_EVENT** at \`$SPOOLWAY_FROM\`"
    echo
    echo '<details><summary>Current task document</summary>'
    echo
    head -c 50000 "$SPOOLWAY_TASK_FILE"
    echo
    echo '</details>'
  } | gh issue comment "$SPOOLWAY_TICKET" -R "$repo" --body-file -
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
  # A state file makes a retried synchronous open idempotent even when GitHub
  # accepted an issue immediately before a later request failed.
  state="$SPOOLWAY_OUT.state"
  epic=$SPOOLWAY_EPIC                       # set when the group already names one
  ticket=
  if [ -f "$state" ]; then
    while IFS= read -r line; do
      case "$line" in
        epic=*) epic=${line#epic=} ;;
        ticket=*) ticket=${line#ticket=} ;;
      esac
    done < "$state"
  fi

  save_state() {
    { echo "epic=$epic"; echo "ticket=$ticket"; } > "$state"
  }

  if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
    set -- gh issue create -R "$repo" -t "$SPOOLWAY_GROUP" \
      -F "$SPOOLWAY_EPIC_BODY" --label spoolway:group
    if same_repo_issue "$SPOOLWAY_SOURCE"; then
      set -- "$@" --parent "$SPOOLWAY_SOURCE"
    fi
    epic=$("$@") || exit $?
    save_state
  fi

  body="$SPOOLWAY_OUT.ticket-body.md"
  {
    cat "$SPOOLWAY_TICKET_BODY"
    echo
    echo '<details><summary>Task document at queue time</summary>'
    echo
    head -c 50000 "$SPOOLWAY_TASK_FILE"
    echo
    echo '</details>'
  } > "$body"

  if [ -z "$ticket" ]; then
    parent=$epic
    if [ -z "$parent" ] && same_repo_issue "$SPOOLWAY_SOURCE"; then
      parent=$SPOOLWAY_SOURCE
    fi
    deps=
    for dep in $SPOOLWAY_DEPENDS_TICKETS; do
      deps="${deps}${deps:+,}$dep"
    done
    set -- gh issue create -R "$repo" -t "$SPOOLWAY_TITLE" \
      -F "$body" --label spoolway:task
    [ -n "$parent" ] && set -- "$@" --parent "$parent"
    [ -n "$deps" ] && set -- "$@" --blocked-by "$deps"
    ticket=$("$@") || exit $?
    save_state
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

[ -n "$SPOOLWAY_TICKET" ] || exit 0

case "$SPOOLWAY_EVENT" in
  queued)
    mark_in_progress
    ;;
  blocked)
    comment_snapshot
    ;;
  paused)
    comment_snapshot
    ;;
  done)
    hand_off_for_review
    ;;
esac
