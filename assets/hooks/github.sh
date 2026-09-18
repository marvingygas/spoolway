#!/bin/sh
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on queued, blocked, paused and
# done.
#
# Every command here was run against a real repository with gh 2.97.0,
# except `gh pr edit --body-file -` and the `done` branch's own `gh issue
# comment`: those two write, and nobody has yet had a throwaway pull
# request to let them write to. Both `gh pr view` forms the `done` branch
# uses — by branch name with `-R`, and by pull request URL for the body —
# were confirmed live and return exactly the shapes read below. `gh` has no
# sub-issue command, so the parent link
# goes through the REST endpoint, which wants the issue's integer id — not
# the node id that `gh issue view --json id` returns.

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

# `done` here is `spoolway stack` having opened this task's own pull
# request, not a merge — nobody has reviewed anything yet, so this never
# closes the ticket. Instead it hands the ticket to that pull request: a
# `Closes #<n>` trailer on the pull request's body is what GitHub reads as a
# closing reference. GitHub only honours that reference automatically when
# the pull request targets the repository's default branch — a stacked
# task's pull request targets an earlier task's own branch instead, so the
# trailer still records the association unambiguously but a repository
# running stacked tasks needs its own merge-time automation to turn that
# record into an actual close. Pushing to the default branch will not do:
# the trailer lives only in the pull request's own body, never in a commit
# message, so nothing about the push itself names the ticket. What works is
# a workflow triggered on that pull request being closed (`pull_request:
# closed`, gated on `github.event.pull_request.merged == true`) that reads
# *that* pull request's own body for its `Closes #<n>` reference and closes
# the ticket explicitly. Fired on every task's own `done`, not only a
# group's last one — a group of one has no epic to fold this into, and
# needs the handoff exactly the same. Every `gh` call below is checked: by
# the time `done` fires, `spoolway stack` has already opened this branch's
# pull request, so a lookup or write failing here is a real problem, never
# a reason to quietly skip the handoff and exit clean. None of the recovery
# text below points the operator at a queue command — `done` is not a step
# a person can advance a task past by hand (see `retry_if_failed`'s own doc
# in `tracking.rs`): a task held here on `issue_tracking.on_fail = pause`
# retries this same hook automatically on the dispatcher's next pass, no
# command needed, once whatever broke is fixed. The shipped default is
# `issue_tracking.on_fail = ignore`, though, which archives the task on a
# failed hook exactly like a passing one — there is no hold to retry at
# all — so every message below also gives the manual fix: add the
# `Closes #<n>` reference, or the comment, by hand.
if [ "$SPOOLWAY_EVENT" = done ] && [ -n "$SPOOLWAY_TICKET" ]; then
  ticket_n=${SPOOLWAY_TICKET##*/}
  if ! pr=$(gh pr view "$SPOOLWAY_BRANCH" -R "$repo" --json url --jq .url); then
    echo "spoolway: \`gh pr view\` failed for \`$SPOOLWAY_BRANCH\` — check that \`gh\` is" \
         "logged in to $repo. A task held on \`issue_tracking.on_fail = pause\` retries this" \
         "automatically once that is fixed; the default \`on_fail = ignore\` already moved" \
         "this task on, so open $SPOOLWAY_TICKET's pull request yourself and add" \
         "\`Closes #$ticket_n\` to its body." >&2
    exit 1
  fi
  if [ -z "$pr" ]; then
    echo "spoolway: \`gh pr view\` found no open pull request for \`$SPOOLWAY_BRANCH\` —" \
         "open one (or re-run \`spoolway stack\`). A task held on" \
         "\`issue_tracking.on_fail = pause\` retries this automatically once one exists;" \
         "otherwise add \`Closes #$ticket_n\` to the new pull request's body yourself." >&2
    exit 1
  fi
  if ! body=$(gh pr view "$pr" --json body --jq .body); then
    echo "spoolway: could not read $pr's body — check \`gh\` access to $repo; nothing was" \
         "changed. A task held on \`issue_tracking.on_fail = pause\` retries this" \
         "automatically once that is fixed; otherwise add \`Closes #$ticket_n\` to $pr's" \
         "body yourself." >&2
    exit 1
  fi
  # A plain substring search would read ticket #1's `Closes #1` as already
  # present inside someone else's `Closes #123` — the bracket alternative
  # below only matches when the number ends exactly there, at the string's
  # end or before a non-digit.
  case "$body" in
    *"Closes #$ticket_n" | *"Closes #$ticket_n"[!0-9]*) ;;
    *)
      trailer=$(printf '\n\nCloses #%s\n' "$ticket_n")
      # `${#var}` counts characters, not bytes, and GitHub's 65,536-byte
      # ceiling is exactly that — bytes — so a body holding anything outside
      # plain ASCII needs `wc -c` here rather than a character count that
      # would silently under-report it. `tr -d` strips the whitespace some
      # `wc` implementations pad a bare count with.
      body_bytes=$(printf '%s' "$body" | wc -c | tr -d '[:space:]')
      trailer_bytes=$(printf '%s' "$trailer" | wc -c | tr -d '[:space:]')
      if [ $((body_bytes + trailer_bytes)) -gt 65536 ]; then
        # GitHub refuses a body over 65,536 bytes outright, and cutting the
        # existing text to make room would risk truncating `spoolway
        # stack`'s own trailer — its conflict/touches list and co-author
        # tag — so this fails loudly rather than silently corrupting the
        # pull request.
        echo "spoolway: $pr's body is already at GitHub's 65,536-byte limit — trim it by hand" \
             "so \`Closes #$ticket_n\` fits. A task held on \`issue_tracking.on_fail = pause\`" \
             "retries this automatically afterward; otherwise add the trailer yourself." >&2
        exit 1
      fi
      if ! printf '%s%s' "$body" "$trailer" | gh pr edit "$pr" --body-file -; then
        # A failed edit leaves the pull request unlinked — reporting success
        # anyway (the comment below) would claim a handoff that never
        # happened, so this stops here instead.
        echo "spoolway: \`gh pr edit\` failed while handing $SPOOLWAY_TICKET off to $pr —" \
             "check \`gh\` access to $repo; the pull request's body was not changed. A task" \
             "held on \`issue_tracking.on_fail = pause\` retries this automatically once that" \
             "is fixed; otherwise add \`Closes #$ticket_n\` to $pr's body yourself." >&2
        exit 1
      fi
      ;;
  esac
  if ! gh issue comment "$SPOOLWAY_TICKET" \
       --body "spoolway: handed off to $pr — awaiting merge and whatever merge-time closure \
this repository has configured."; then
    echo "spoolway: \`gh issue comment\` failed on $SPOOLWAY_TICKET after handing it off to" \
         "$pr — $pr already links $SPOOLWAY_TICKET, so only the comment is missing. Check" \
         "\`gh\` access to $repo. A task held on \`issue_tracking.on_fail = pause\` retries" \
         "this automatically, redoing nothing since the link already exists; otherwise" \
         "comment on $SPOOLWAY_TICKET yourself." >&2
    exit 1
  fi
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
