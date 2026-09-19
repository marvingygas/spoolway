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
# the parent of the group issue.
same_repo_issue() {
  case "$1" in
    *"/$repo/issues/"[0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

# One section's own content out of a task file, matching the boundary rule
# `Task::find_section` uses in src/task.rs: a line equal to `heading` (case
# folded, trailing space trimmed) starts it, and it ends at the next line
# whose own leading `#`s number 1 through heading's own level — a
# sub-heading nested deeper stays inside the section instead of ending it.
# Leading and trailing blank lines are trimmed off what is printed. Prints
# nothing at all for a missing section, a missing file, or a blank
# `$SPOOLWAY_TASK_FILE` — the `open` event may carry the empty string there
# for a document with no file of its own (see `tracking.rs`'s own doc on
# `open_env`).
extract_section() {
  file=$1
  heading=$2
  [ -n "$file" ] && [ -f "$file" ] || return 0
  awk -v heading="$heading" '
    function hashes(l,    n) {
      n = 0
      while (substr(l, n + 1, 1) == "#") n++
      return n
    }
    BEGIN { level = hashes(heading); found = 0; n = 0 }
    {
      line = $0
      sub(/[ \t\r]+$/, "", line)
      if (!found) {
        if (tolower(line) == tolower(heading)) found = 1
        next
      }
      lvl = hashes(line)
      if (lvl > 0 && lvl <= level) exit
      buf[++n] = line
    }
    END {
      start = 1
      while (start <= n && buf[start] == "") start++
      last = n
      while (last >= start && buf[last] == "") last--
      for (i = start; i <= last; i++) print buf[i]
    }
  ' "$file"
}

# `heading` and its content from `$SPOOLWAY_TASK_FILE`, blank-line-separated
# the way a document's own headings are — or nothing when the document has
# no such section, so the ticket body never shows an empty one.
ticket_section() {
  content=$(extract_section "$SPOOLWAY_TASK_FILE" "$1")
  [ -n "$content" ] && printf '%s\n\n%s\n\n' "$1" "$content"
}

# The same, but tight against its heading — `## Status Log` and `##
# Handoff` are already bulleted lists in the document, with no blank line
# under the heading, and a comment reproduces that instead of inventing one.
comment_section() {
  content=$(extract_section "$SPOOLWAY_TASK_FILE" "$1")
  [ -n "$content" ] && printf '%s\n%s\n\n' "$1" "$content"
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

# A `blocked` or `paused` comment carries only what just changed — the log
# of steps taken and whatever the last one is handing forward — never the
# whole document behind it.
comment_snapshot() {
  {
    echo "**spoolway** — \`$SPOOLWAY_TASK\` is **$SPOOLWAY_EVENT** at \`$SPOOLWAY_FROM\`"
    echo
    comment_section "## Status Log"
    comment_section "## Handoff"
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

  # A group issue opens whatever the group's size — a group of one gets one
  # too, rather than folding its lone task straight under `$SPOOLWAY_SOURCE`:
  # one issue shape for every group, not two. `$SPOOLWAY_GROUP_DESCRIPTION`
  # is never blank here: `parse_submission` (queue.rs:730) already refuses
  # any document with no `group:` before it ever reaches the open hook, so
  # every task that gets here has a named group, and `require_group_
  # description` (queue.rs:890) in turn refuses a submission whose group
  # sets no `group_description:` on any of its documents.
  if [ -z "$epic" ]; then
    epic_title=$(printf '%s\n' "$SPOOLWAY_GROUP_DESCRIPTION" | head -n 1)
    epic_lead=$(printf '%s\n' "$SPOOLWAY_GROUP_DESCRIPTION" | tail -n +2)
    epic_body="$SPOOLWAY_OUT.epic-body.md"
    {
      [ -n "$epic_lead" ] && printf '%s\n\n' "$epic_lead"
      cat "$SPOOLWAY_EPIC_BODY"
    } > "$epic_body"
    set -- gh issue create -R "$repo" -t "$epic_title" \
      -F "$epic_body" --label spoolway:group
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
    ticket_section "## Intend"
    ticket_section "## Context"
    ticket_section "## Acceptance criteria"
  } > "$body"

  if [ -z "$ticket" ]; then
    deps=
    for dep in $SPOOLWAY_DEPENDS_TICKETS; do
      deps="${deps}${deps:+,}$dep"
    done
    set -- gh issue create -R "$repo" -t "$SPOOLWAY_TITLE" \
      -F "$body" --label spoolway:task --parent "$epic"
    [ -n "$deps" ] && set -- "$@" --blocked-by "$deps"
    ticket=$("$@") || exit $?
    save_state
  fi

  # The short handle spoolway puts in generated names, and the issue's web
  # address kept on the task for later use — spoolway stores and validates
  # `url=` but shows it nowhere yet. `$epic` is already a URL here: take the
  # issue number off the end, with a `gh-` prefix so the slug starts with a
  # letter the way every spoolway id does. Every group now has an epic, so
  # it is always the key — never the ticket's own.
  key=$epic
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
