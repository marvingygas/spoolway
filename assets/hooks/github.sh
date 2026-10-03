#!/usr/bin/env bash
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on started, blocked, paused and
# done.
#
# spoolway-requires: bash >= 3.2
# spoolway-requires: gh >= 2.97.0
#
# 3.2 is stock macOS's own bash, not a round number picked for looks: the
# trap below needs nothing newer — `FUNCNAME`, `BASH_SUBSHELL` and the ERR
# trap itself are all at least that old — so this asks for the oldest bash
# that still runs it, not the newest one happened to be tested against.
#
# Every command here was checked against that version. The hook treats
# GitHub as a mirror: issue failures are reported back to spoolway, while
# config decides whether they should pause delivery.
#
# `set -eE` plus the ERR trap below means a command that fails — outside an
# `if`, a `&&`/`||` list, or one explicitly marked `|| true` — stops the
# hook and prints the command, its line, the case arm and the call chain
# that failed, to this run's own log — the same log a paused task's
# `## Hook error` reads its tail from. There is no `|| exit $?` left
# anywhere below: bash never fires an ERR trap for a command inside an
# `||` list, so that old pattern would have hidden the very trace this is
# for; a call that is meant to be allowed to fail instead carries an
# explicit `|| true`.

set -eE
trap 'hook_trace "$LINENO" "$BASH_COMMAND"' ERR

# Prints what failed and where, then exits with the failing command's own
# code — the one thing `set -e` would otherwise do silently. `FUNCNAME`
# already holds this trap handler's own frame at index 0 when it runs, so
# the chain read out below starts one past it; nothing left in the chain
# means the failing command was never inside a function at all, i.e. a
# case arm running at the top level of the script, printed as `main`.
#
# A command substitution (`x=$(...)`) runs in its own subshell, and this
# trap is inherited into it — so a call failing inside one fires here
# twice: once for the real command, deep in the subshell, and once more
# for the assignment itself once the subshell's own non-zero exit reaches
# it. `$BASH_SUBSHELL` is what tells the two apart — greater than zero
# inside the subshell — and it is the deeper, inner firing that names the
# actual failing command, so that is the one that exits quietly rather
# than the one this prints from; the outer firing, at the assignment, is
# what a person actually sees, one trace, naming the whole `x=$(...)` line.
#
# `$BASH_COMMAND` is the failing command's own source text, unexpanded —
# `"$SPOOLWAY_TICKET"`, not the ticket id it actually held. bash has no
# built-in that hands back the expanded argv of the command that just
# failed, and reconstructing one by re-expanding the source text risks
# reading it wrong or re-running a side effect a command substitution
# inside it already had. The line number is real, and the source text is
# usually enough to find the call in the file next to it.
hook_trace() {
  code=$?
  line=$1
  cmd=$2
  if [ "$BASH_SUBSHELL" -gt 0 ]; then
    exit "$code"
  fi
  printf '%s: command failed (exit %d)\n' "$(basename "$0")" "$code" >&2
  printf '  at line %s: %s\n' "$line" "$cmd" >&2
  printf '  in case arm: %s\n' "${SPOOLWAY_EVENT:-}" >&2
  callers=
  i=1
  while [ "$i" -lt "${#FUNCNAME[@]}" ]; do
    callers="${callers:+$callers -> }${FUNCNAME[$i]}"
    i=$((i + 1))
  done
  printf '  called from %s\n' "${callers:-main}" >&2
  exit "$code"
}

repo=$SPOOLWAY_PROJECT_KEY                # `[issue_tracking] project_key`

# Whether a source is an issue in this repository. A matching source becomes
# the parent of the group issue.
same_repo_issue() {
  case "$1" in
    *"/$repo/issues/"[0-9]*) return 0 ;;
    *) return 1 ;;
  esac
}

# `$SPOOLWAY_LABELS`, comma-joined, one label per line — `queue add` already
# refuses one holding whitespace or a comma (src/commands/queue.rs), so a
# plain split on `,` is all this needs.
each_label() {
  [ -n "$SPOOLWAY_LABELS" ] || return 0
  old_ifs=$IFS
  IFS=,
  for label in $SPOOLWAY_LABELS; do
    IFS=$old_ifs
    [ -n "$label" ] && printf '%s\n' "$label"
  done
  IFS=$old_ifs
}

# `gh label create` for every label `$SPOOLWAY_LABELS` names that `gh label
# list` does not already show for this repository — never `--force`, so a
# label that already exists keeps whatever colour or description it already
# has. Run once, before either issue is created, so both can carry every
# label from the moment they exist.
#
# `--limit 1000` because `gh label list` defaults to 30 — a repository
# carrying more than that would otherwise read a real, older label as
# missing, `gh label create` would then refuse it as a duplicate, and the
# whole `open` event would fail over a label that was never actually
# missing. The match itself is case-insensitive: GitHub treats a label's
# name that way too, so `Bug` against an existing `bug` is the same label,
# not a new one.
create_missing_labels() {
  [ -n "$SPOOLWAY_LABELS" ] || return 0
  existing=$(gh label list -R "$repo" -L 1000 --json name --jq '.[].name')
  missing=$(each_label | while IFS= read -r label; do
    printf '%s\n' "$existing" | grep -qiFx "$label" || printf '%s\n' "$label"
  done)
  [ -n "$missing" ] || return 0
  # A `<<` heredoc, not `printf ... | while ...`. A pipe would still stop
  # the hook — the loop is the pipeline's last element, so its status is
  # the pipeline's — but it runs the loop in a subshell, where the ERR trap
  # exits quietly, and the trace printed at this level would then
  # read `at line N: printf ...`, naming the pipe's first command rather
  # than the `gh label create` that failed (seen 2026-09-30 in review). A
  # heredoc keeps the loop in this shell, so the trace names the create.
  while IFS= read -r label; do
    [ -n "$label" ] || continue
    gh label create "$label" -R "$repo"
  done <<LABELS
$missing
LABELS
}

# `$1` (`--label` or `--add-label`) once per label in `$SPOOLWAY_LABELS`,
# meant to be splatted unquoted into a `gh` call: `$(label_flags --label)`.
# Splitting the substitution on whitespace is safe here — `each_label`'s own
# labels can hold none.
label_flags() {
  each_label | while IFS= read -r label; do
    printf '%s\n%s\n' "$1" "$label"
  done
}

# One section's own content out of a task file, matching the boundary rule
# `Task::find_section` uses in src/task.rs: a line equal to `heading` (case
# folded, trailing space trimmed) starts it, and it ends at the next line
# whose own leading `#`s number 1 through heading's own level — a
# sub-heading nested deeper stays inside the section instead of ending it.
# Leading and trailing blank lines are trimmed off what is printed. Prints
# nothing at all for a missing section, a missing file, or a blank
# `$SPOOLWAY_TASK_FILE` — the `open` event may carry the empty string there
# for a task with no file of its own (see `tracking.rs`'s own doc on
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
# the way a task's own headings are — or nothing when the task has
# no such section, so the ticket body never shows an empty one.
#
# `if`/`fi`, not `[ -n "$content" ] && printf ...`: a missing section is
# the ordinary case, not a failure, but that shape makes an empty match this
# function's own last, failing command — harmless under plain `sh`, fatal
# under `set -e` once this runs bare inside `{ ... } > "$body"` below, where
# nothing wraps the call in an `if` or an `&&`/`||` of its own.
ticket_section() {
  content=$(extract_section "$SPOOLWAY_TASK_FILE" "$1")
  if [ -n "$content" ]; then
    printf '%s\n\n%s\n\n' "$1" "$content"
  fi
}

# The same, but tight against its heading — `## Status Log` and `##
# Handoff` are already bulleted lists in the task, with no blank line
# under the heading, and a comment reproduces that instead of inventing one.
comment_section() {
  content=$(extract_section "$SPOOLWAY_TASK_FILE" "$1")
  if [ -n "$content" ]; then
    printf '%s\n%s\n\n' "$1" "$content"
  fi
}

# `$SPOOLWAY_TITLE` with a leading commit prefix taken off, for the title of
# the Sub-task or task issue alone — the Story or group issue always keeps
# the group's own name. The prefix is a lowercase word, an optional
# `(scope)`, and an optional `!`, the shape this project's own commit
# convention uses (`perf(status): …`, `fix!: …`); a title with none of that
# passes through unchanged. This can eat a real title that happens to start
# the same way (`vendor: drop the old client` reads as a prefix), a trade-off
# the plan accepted rather than invent a second, narrower pattern.
strip_title_prefix() {
  printf '%s' "$1" | sed -E 's/^[a-z]+(\([^()]*\))?!?: //'
}

# GitHub issues only have open/closed as native states. This label means the
# task has entered spoolway; blocked and paused are comments instead of state
# labels because spoolway has no matching event when either condition clears.
mark_in_progress() {
  gh issue edit "$SPOOLWAY_TICKET" -R "$repo" \
    --add-label spoolway:in-progress
}

# `done` means spoolway handed the task to a pull request, not that the
# change merged. Nothing is posted on the pull request itself — nobody wants
# a comment there — the link runs the other way instead: the ticket names
# the pull request, so a project's own merge automation can read it off the
# ticket to tell a group's siblings apart and check whether each one's own
# pull request has merged. Closing is entirely that automation's job, never
# this hook's — it only relabels the ticket for review. This repository's own
# merge workflow, `.github/workflows/spoolway-issues.yml`, is not this file
# and not spoolway's own binary; it is this project's to keep in step with
# whatever this comment says.
hand_off_for_review() {
  pr=$(gh pr view "$SPOOLWAY_BRANCH" -R "$repo" --json url --jq .url)
  [ -n "$pr" ] || {
    echo "github.sh: no pull request found for $SPOOLWAY_BRANCH" >&2
    exit 1
  }

  gh issue edit "$SPOOLWAY_TICKET" -R "$repo" \
    --remove-label spoolway:in-progress \
    --add-label spoolway:review

  gh issue comment "$SPOOLWAY_TICKET" -R "$repo" --body \
    "Ready for review in $pr"
}

# A `blocked` or `paused` comment carries only what just changed — the log
# of steps taken and whatever the last one is handing forward — never the
# whole task behind it.
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

  # Every label this task names has to exist before either issue below can
  # be created or edited with it.
  create_missing_labels

  # A group issue opens whatever the group's size — a group of one gets one
  # too, rather than folding its lone task straight under `$SPOOLWAY_SOURCE`:
  # one issue shape for every group, not two. `$SPOOLWAY_GROUP_DESCRIPTION`
  # is never blank here: `parse_submission` (queue.rs:730) already refuses
  # any task with no `group:` before it ever reaches the open hook, so
  # every task that gets here has a named group, and `require_group_
  # description` (queue.rs:890) in turn refuses a submission whose group
  # sets no `group_description:` on any of its tasks.
  if [ -z "$epic" ]; then
    epic_title=$SPOOLWAY_GROUP
    # A `group_description: |` block scalar keeps its own trailing newline
    # all the way through (queue.rs:1579 stores it verbatim); command
    # substitution strips that, so the blank line below is always exactly
    # one regardless of whether the author wrote `|`, `|-` or a plain
    # scalar. The body is the description alone, flat — the whole of what
    # the hook itself adds to a group issue.
    epic_lead=$(printf '%s\n' "$SPOOLWAY_GROUP_DESCRIPTION")
    epic_body="$SPOOLWAY_OUT.epic-body.md"
    printf '%s\n' "$epic_lead" > "$epic_body"
    set -- gh issue create -R "$repo" -t "$epic_title" \
      -F "$epic_body" --label spoolway:group $(label_flags --label)
    if same_repo_issue "$SPOOLWAY_SOURCE"; then
      set -- "$@" --parent "$SPOOLWAY_SOURCE"
    fi
    epic=$("$@")
    save_state
  elif [ -n "$SPOOLWAY_LABELS" ]; then
    # The epic already exists — a task queued after the group's first still
    # carries its own labels onto it, so the group issue ends up with the
    # union of every task's labels rather than just the first task's.
    gh issue edit "$epic" -R "$repo" $(label_flags --add-label)
  fi

  # The task's own words alone: its `## Context` and `## Acceptance
  # criteria`, never `## Intend` — that section is the plan's framing of the
  # task, not the task itself.
  body="$SPOOLWAY_OUT.ticket-body.md"
  {
    ticket_section "## Context"
    ticket_section "## Acceptance criteria"
  } > "$body"

  ticket_title=$(strip_title_prefix "$SPOOLWAY_TITLE")

  if [ -z "$ticket" ]; then
    deps=
    for dep in $SPOOLWAY_DEPENDS_TICKETS; do
      deps="${deps}${deps:+,}$dep"
    done
    set -- gh issue create -R "$repo" -t "$ticket_title" \
      -F "$body" --label spoolway:task --parent "$epic" $(label_flags --label)
    [ -n "$deps" ] && set -- "$@" --blocked-by "$deps"
    ticket=$("$@")
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
  started)
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
