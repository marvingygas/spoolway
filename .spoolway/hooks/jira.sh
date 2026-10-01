#!/usr/bin/env bash
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on started, blocked, paused and
# done.
#
# spoolway-requires: bash >= 3.2
# spoolway-requires: acli >= 1.3.39
# spoolway-requires: jq >= 1.6
#
# 3.2 is stock macOS's own bash, not a round number picked for looks: the
# trap below needs nothing newer — `FUNCNAME`, `BASH_SUBSHELL` and the ERR
# trap itself are all at least that old — so this asks for the oldest bash
# that still runs it, not the newest one happened to be tested against.
#
# Every `acli` command below was checked against that version, live, against
# a real Jira site — including one break between it and the 1.3.30 floor the
# project shipped with: `workitem view` used to take its key as `--key`, and
# now takes it positionally (`workitem view KEY-1`); every call below already
# reads that way. Two more things a site can still differ on, and each is a
# one-line edit at the head of this file if yours does: that `Blocks` and
# `Relates` are your project's own spelling for "this one comes first" and
# "this is the same piece of work as that", and that the three status names
# match the ones `acli` sees. Jira answers in the language of the account
# `acli` is logged in as: a German account sees `Entwurf` and `In Arbeit`
# where this file says `Draft` and `In Progress`, and a transition asked for
# by the English name finds nothing to move to. The live proof this shipped
# with first ran under a German account, and read that as a workflow with no
# route from Draft into In Progress; switching the account to English (at
# id.atlassian.com, account preferences) made every transition here work.
#
# The task file itself is deliberately not sent. Jira's REST API would take
# it as a real attachment, but only against a site, an account email and an
# API token — a second set of credentials for spoolway to hold, which this
# project would rather not. `acli` cannot upload one on its own: its
# `workitem attachment` group only lists and deletes. So the comment names
# the task and leaves the file where it already is, in the queue.
#
# `jq` is a hard dependency alongside `acli`: `workitem create --json` is the
# only way this gets a ticket's key back, and there is no acli flag that
# skips the JSON envelope. Install it beside `acli` before naming this hook.
# A create call that comes back with no key — `jq` missing, `acli` failing,
# the JSON shape changing underfoot — exits loudly rather than writing an
# empty `epic=`/`ticket=` line: a lost key is a ticket nothing ever links to
# again, and this codebase's error style has no room for losing that quietly.
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
# `--key "$SPOOLWAY_TICKET"`, not the key it actually held. bash has no
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

project=$SPOOLWAY_PROJECT_KEY             # `[issue_tracking] project_key`

# The work item types and link types this project's Jira site names — a
# company-managed project spells the sub-task type `Sub-task`, not
# `Subtask`, so this is the one edit a project like that needs to make here.
# The three status names below are the site's own workflow names, not
# spoolway's — nothing proves them ahead of time any more; a name this site
# does not have surfaces as a failed transition on the real event that hits
# it, with the trace naming the call that failed. `open` never transitions
# a new Story or Sub-task into `status_draft` itself — it relies on that
# being whatever status Jira already creates a new work item into, so
# `status_draft` has to name that status exactly, or a Story that starts
# somewhere else never reads as "still Draft" for `started`'s own check
# below, and never leaves it.
type_story=Story
type_subtask=Subtask
link_blocks=Blocks
link_relates=Relates
status_draft=Draft
status_progress="In Progress"
status_review=Review

# The browse host this account's site actually answers on — `acli jira auth
# status` prints it as `  Site: <host>`. `workitem view`'s own `.self` field
# names the API backend instead (`jira-prod-eu-43-4.prod.atl-paas.net` on the
# site this hook was proven against), which is not the same host a browser
# can reach at all; building a `url=` answer or a comment link from `.self`
# would hand back a link nothing resolves. `auth status` is the one command
# here that already knows the difference.
site_host() {
  acli jira auth status 2>/dev/null | sed -n 's/^ *Site: //p'
}

# Union of two comma-joined label lists, deduplicated, comma-joined again.
# `acli workitem edit --labels` might add to a Story's labels or might
# replace them outright — behaviour this hook was proven against did the
# former, but the risk the plan called out is the latter, so this always
# reads the Story's own labels first and writes back everything either way.
union_labels() {
  # `%s\n`, not `%s` — a label list with no comma in it (the common case,
  # one label) leaves `tr` nothing to split on, so its output carries no
  # trailing newline of its own; without one here, that output runs
  # straight into the next `printf`'s with no separator between them at
  # all, silently welding two real labels into one word that matches
  # neither (live proof: "scanner" + "scanner" read back "scannerscanner").
  { printf '%s\n' "$1" | tr ',' '\n'; printf '%s\n' "$2" | tr ',' '\n'; } |
    sed '/^$/d' | sort -u | tr '\n' ',' | sed 's/,$//'
}

# Hangs $2 (an issue this hook just created, by its own key) as a Jira link
# under $1, doing nothing at all unless $1 is a `.../browse/<key>` URL for
# this same $project. $1 is SPOOLWAY_SOURCE most of the time, and spoolway
# never parses that field — most of the time it names a plan page's path,
# not an issue, and this is what lets it fall straight through untouched.
hang_under() {
  child=$2
  [ -z "$child" ] && return 0
  case "$1" in
    */browse/"$project"-[0-9]*) ;;
    *) return 0 ;;
  esac
  # Best-effort, same as before this hook ever traced a failure: a link that
  # does not take leaves the new issue exactly as good, just not hung under
  # its source — worth trying, never worth pausing a task over.
  acli jira workitem link create --out "${1##*/}" --in "$child" --type "$link_relates" --yes \
    || true
}

if [ "$SPOOLWAY_EVENT" = fetch ]; then
  fields=$(acli jira workitem view "$SPOOLWAY_REF" --fields "summary,status,labels,description" \
             --json) && [ -n "$fields" ] || {
    echo "jira.sh: acli returned nothing for \"$SPOOLWAY_REF\" — does the key exist?" >&2
    exit 1
  }
  # `comment list --json` answers `{comments: [...], isLast, maxResults, ...}`,
  # not a bare array — `$c.comments[]?`, not `$c[]?`, is what live proof
  # against this site found: the bare form iterated the *object's* own
  # values instead, one of which is the comments array itself, and choked
  # trying to read `.author` off it.
  # Allowed to fail: a Jira site with comments disabled, or a transient
  # error here, should still answer the issue's own fields — the fallback
  # below reads as "no comments" rather than losing the whole fetch over it.
  comments=$(acli jira workitem comment list --key "$SPOOLWAY_REF" --json 2>/dev/null) || true
  [ -n "$comments" ] || comments='{"comments":[]}'
  # Jira Cloud's v3 API sends a description and a comment body as an
  # Atlassian Document Format tree, not text — `adf` below flattens one into
  # plain text, a paragraph per blank-line-separated block and `- ` per list
  # item, so whoever reads `body` gets prose rather than a nested object. A
  # plain string passes through unchanged. A comment's author is
  # `displayName` on Cloud, which no longer sends `name` at all; the old
  # `.author.name // .author` fallback handed back the whole author object.
  jq -n --argjson f "$fields" --argjson c "$comments" --arg site "$(site_host)" '
  def adf:
    if type == "string" then .
    elif type == "array" then map(adf) | join("")
    elif type != "object" then ""
    elif .type == "text" then .text // ""
    elif .type == "hardBreak" then "\n"
    elif .type == "mention" or .type == "emoji" then .attrs.text // ""
    elif .type == "inlineCard" or .type == "blockCard" then .attrs.url // ""
    elif .type == "listItem" then "- " + ((.content // []) | adf)
    elif .type == "codeBlock" then "```\n" + ((.content // []) | adf) + "\n```\n\n"
    elif .type == "rule" then "---\n\n"
    elif .type == "paragraph" or .type == "heading" or .type == "blockquote" then
      ((.content // []) | adf) + "\n\n"
    else (.content // []) | adf
    end;
  def text: adf | gsub("\n{3,}"; "\n\n") | sub("\\s+$"; "");
  {
    ref: $f.key,
    url: ("https://" + $site + "/browse/" + $f.key),
    title: $f.fields.summary,
    state: ($f.fields.status.name // "" | ascii_downcase),
    labels: ($f.fields.labels // []),
    body: ($f.fields.description // "" | text),
    comments: [$c.comments[]? | {
      author: (.author | if type == "object" then .displayName // .name // "" else . // "" end),
      body: (.body // "" | text)
    }]
  }' > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT" = open ]; then
  # A state file makes a retried synchronous open idempotent even when Jira
  # accepted an issue immediately before a later request failed — the same
  # shape `github.sh` keeps for the same reason. `SPOOLWAY_EPIC` already
  # carries the group's Story key forward once an earlier task in the batch
  # has answered; this file only guards a retry of *this* task's own open.
  state="$SPOOLWAY_OUT.state"
  epic=$SPOOLWAY_EPIC                       # set when the group already opened its Story
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

  if [ -z "$epic" ]; then
    # A group of one still opens a Story — one shape for every group, not a
    # Story only once a second task shows up.
    epic=$(acli jira workitem create --project "$project" --type "$type_story" \
             --summary "$SPOOLWAY_GROUP" --description-file "$SPOOLWAY_EPIC_BODY" \
             ${SPOOLWAY_LABELS:+--label "$SPOOLWAY_LABELS"} --json | jq -r '.key // empty')
    if [ -z "$epic" ]; then
      echo "jira.sh: acli/jq returned no Story key — is jq installed, and did \`workitem create\` succeed?" >&2
      exit 1
    fi
    hang_under "$SPOOLWAY_SOURCE" "$epic"
    save_state
  elif [ -n "$SPOOLWAY_LABELS" ]; then
    # The Story already exists — a task queued after the group's first still
    # carries its own labels onto it, so the Story ends up with the union of
    # every task's labels rather than just the first task's.
    existing=$(acli jira workitem view "$epic" --fields labels --json | jq -r '.fields.labels | join(",")')
    acli jira workitem edit --key "$epic" --labels "$(union_labels "$existing" "$SPOOLWAY_LABELS")" \
      --yes
  fi

  if [ -z "$ticket" ]; then
    ticket=$(acli jira workitem create --project "$project" --type "$type_subtask" \
               --summary "$SPOOLWAY_TITLE" --description-file "$SPOOLWAY_TICKET_BODY" \
               --parent "$epic" ${SPOOLWAY_LABELS:+--label "$SPOOLWAY_LABELS"} \
               --json | jq -r '.key // empty')
    if [ -z "$ticket" ]; then
      echo "jira.sh: acli/jq returned no Sub-task key — is jq installed, and did \`workitem create\` succeed?" >&2
      exit 1
    fi
    # The Sub-task is already real once its key comes back — saved before
    # the Blocks links below are even attempted, so a retry after either of
    # them fails resumes from the state file instead of creating a second
    # Sub-task for the same task (review finding: under `set -eE` a failed
    # link here used to abort before this ever ran).
    save_state
    # Best-effort, the same as `hang_under`'s own Relates link: a Blocks
    # link that does not take leaves the Sub-task exactly as real, just
    # without that one relation — worth trying, never worth losing the
    # ticket's own key over.
    for dep in $SPOOLWAY_DEPENDS_TICKETS; do   # Jira has real issue links
      acli jira workitem link create --out "$dep" --in "$ticket" --type "$link_blocks" --yes \
        || true
    done
  fi

  # The short handle spoolway puts in generated names, and the issue's web
  # address kept on the task for later use — spoolway stores and validates
  # `url=` but shows it nowhere yet. `slug=` is just the Story's key
  # lowercased — `KAN-10` becomes `kan-10`.
  slug=$(printf '%s' "$epic" | tr 'A-Z' 'a-z')
  { echo "epic=$epic"; echo "ticket=$ticket"
    echo "slug=$slug"; echo "url=https://$(site_host)/browse/$epic"; } > "$SPOOLWAY_OUT"
  exit 0
fi

[ -n "$SPOOLWAY_TICKET" ] || exit 0

# A task queued before this project moved to Jira still carries a GitHub
# issue URL as its ticket — skip it, since acli can never resolve that key.
case "$SPOOLWAY_TICKET" in
  "$project"-[0-9]*) ;;
  *) exit 0 ;;
esac

case "$SPOOLWAY_EVENT" in
  started)
    # The Sub-task always moves. The Story only leaves Draft once — its
    # status is read first, so a second Sub-task starting in the same group
    # never re-fires a transition a first one already made. Both transitions
    # below are bare: `set -eE` and the trap at the top of this file are what
    # now exits loudly on a real failure, tracing the command that failed —
    # without that, a failed Sub-task move was masked by the `if` around the
    # Story's own check landing last and exiting 0 regardless.
    acli jira workitem transition --key "$SPOOLWAY_TICKET" --status "$status_progress" --yes
    if [ -n "$SPOOLWAY_EPIC" ]; then
      # Allowed to fail: this search is only ever a best-effort read of
      # whether the Story has already left Draft, never the reason a
      # `started` event should pause a task — `still_draft` reading empty
      # here just means `[ "$still_draft" = true ]` below is false, the same
      # as an ordinary "already moved on" answer.
      still_draft=$(acli jira workitem search \
        --jql "project = $project AND key = $SPOOLWAY_EPIC AND status = \"$status_draft\"" \
        --json 2>/dev/null | jq 'length > 0' 2>/dev/null) || true
      # `if`/`fi`, not `[ ... ] &&` — that would leave the whole `started`
      # case arm exiting 1 the moment the Story has already left Draft,
      # which is every started after the group's first: `&&` short-circuits
      # on the false test, and that false exit status is what the arm, and
      # so the whole script, exits with. This paused every later Sub-task's
      # own started event on a live run.
      if [ "$still_draft" = true ]; then
        acli jira workitem transition --key "$SPOOLWAY_EPIC" --status "$status_progress" --yes
      fi
    fi
    ;;
  blocked | paused)
    acli jira workitem comment create --key "$SPOOLWAY_TICKET" --body \
      "spoolway - $SPOOLWAY_TASK is $SPOOLWAY_EVENT at $SPOOLWAY_FROM"
    ;;
  done)
    acli jira workitem transition --key "$SPOOLWAY_TICKET" --status "$status_review" --yes
    if [ "$SPOOLWAY_GROUP_LAST" = 1 ] && [ -n "$SPOOLWAY_EPIC" ]; then
      acli jira workitem transition --key "$SPOOLWAY_EPIC" --status "$status_review" --yes
    fi
    ;;
esac
