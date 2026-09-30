#!/bin/sh
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, `check` from `spoolway doctor` and
# once more as the dispatcher starts, then on started, blocked, paused and
# done.
#
# spoolway-requires: acli >= 1.3.39
# spoolway-requires: jq >= 1.6
#
# Every `acli` command below was checked against that version, live, against
# a real Jira site — including one break between it and the 1.3.30 floor the
# project shipped with: `workitem view` used to take its key as `--key`, and
# now takes it positionally (`workitem view KEY-1`); every call below already
# reads that way. Two more things a site can still differ on, and each is a
# one-line edit at the head of this file if yours does: that `Blocks` and
# `Relates` are your project's own spelling for "this one comes first" and
# "this is the same piece of work as that", and that your workflow actually
# wires up a transition into whatever `status_progress` names — the live
# proof this shipped with found a Jira site whose board draws an "In
# Progress" column that no transition ever reaches from Draft; the hook
# still asks for it, same as any other status here, and a `started` event
# against a workflow like that is a silent no-op rather than a failure. The
# `check` branch, below, only proves each status name exists somewhere on
# the site — it cannot prove a workflow actually reaches it, since that
# takes an issue already sitting one transition away to test with.
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

project=$SPOOLWAY_PROJECT_KEY             # `[issue_tracking] project_key`

# The work item types and link types this project's Jira site names — a
# company-managed project spells the sub-task type `Sub-task`, not
# `Subtask`, so this is the one edit a project like that needs to make here.
# The three status names below are the site's own workflow names, not
# spoolway's: `check` proves each exists, live, before any task can ever
# pause on a hook that would otherwise fail mid-run. `open` never transitions
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
  acli jira workitem link create --out "${1##*/}" --in "$child" --type "$link_relates" --yes
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
  comments=$(acli jira workitem comment list --key "$SPOOLWAY_REF" --json 2>/dev/null)
  [ -n "$comments" ] || comments='{"comments":[]}'
  jq -n --argjson f "$fields" --argjson c "$comments" --arg site "$(site_host)" '{
    ref: $f.key,
    url: ("https://" + $site + "/browse/" + $f.key),
    title: $f.fields.summary,
    state: ($f.fields.status.name // "" | ascii_downcase),
    labels: ($f.fields.labels // []),
    body: ($f.fields.description // ""),
    comments: [$c.comments[]? | {author: (.author.name // .author // ""), body: (.body // "")}]
  }' > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT" = check ]; then
  acli jira auth status >/dev/null 2>&1 || {
    echo "jira.sh check: acli is not logged in — run \`acli jira auth login\`" >&2
    exit 1
  }
  project_json=$(acli jira project view --key "$project" --json) || {
    echo "jira.sh check: project \"$project\" not found, or acli cannot see it" >&2
    exit 1
  }
  printf '%s' "$project_json" | jq -e --arg t "$type_story" 'any(.issueTypes[]?; .name == $t)' \
    >/dev/null 2>&1 || {
    echo "jira.sh check: work item type \"$type_story\" does not exist in project $project — \
set type_story at the head of the hook" >&2
    exit 1
  }
  printf '%s' "$project_json" | jq -e --arg t "$type_subtask" 'any(.issueTypes[]?; .name == $t)' \
    >/dev/null 2>&1 || {
    echo "jira.sh check: work item type \"$type_subtask\" does not exist in project $project — \
set type_subtask at the head of the hook" >&2
    exit 1
  }
  for pair in "status_draft=$status_draft" "status_progress=$status_progress" \
              "status_review=$status_review"; do
    name=${pair%%=*}
    value=${pair#*=}
    acli jira workitem search --jql "project = $project AND status = \"$value\"" >/dev/null 2>&1 || {
      echo "jira.sh check: status \"$value\" does not exist in project $project — set $name \
at the head of the hook" >&2
      exit 1
    }
  done
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
      --yes || exit $?
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
    for dep in $SPOOLWAY_DEPENDS_TICKETS; do   # Jira has real issue links
      acli jira workitem link create --out "$dep" --in "$ticket" --type "$link_blocks" --yes
    done
    save_state
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

case "$SPOOLWAY_EVENT" in
  started)
    # The Sub-task always moves. The Story only leaves Draft once — its
    # status is read first, so a second Sub-task starting in the same group
    # never re-fires a transition a first one already made. Both transitions
    # exit loudly on a real failure (`|| exit $?`, the same rule `github.sh`
    # follows) — without it, a failed Sub-task move was masked by the `if`
    # around the Story's own check landing last and exiting 0 regardless.
    acli jira workitem transition --key "$SPOOLWAY_TICKET" --status "$status_progress" --yes \
      || exit $?
    if [ -n "$SPOOLWAY_EPIC" ]; then
      still_draft=$(acli jira workitem search \
        --jql "project = $project AND key = $SPOOLWAY_EPIC AND status = \"$status_draft\"" \
        --json 2>/dev/null | jq 'length > 0' 2>/dev/null)
      # `if`/`fi`, not `[ ... ] &&` — that would leave the whole `started`
      # case arm exiting 1 the moment the Story has already left Draft,
      # which is every started after the group's first: `&&` short-circuits
      # on the false test, and that false exit status is what the arm, and
      # so the whole script, exits with. This paused every later Sub-task's
      # own started event on a live run, though never on KAN's own site,
      # since KAN's Story never actually left Draft to begin with (see the
      # header note on `status_progress`).
      if [ "$still_draft" = true ]; then
        acli jira workitem transition --key "$SPOOLWAY_EPIC" --status "$status_progress" --yes \
          || exit $?
      fi
    fi
    ;;
  blocked | paused)
    acli jira workitem comment create --key "$SPOOLWAY_TICKET" --body \
      "spoolway - $SPOOLWAY_TASK is $SPOOLWAY_EVENT at $SPOOLWAY_FROM"
    ;;
  done)
    acli jira workitem transition --key "$SPOOLWAY_TICKET" --status "$status_review" --yes \
      || exit $?
    if [ "$SPOOLWAY_GROUP_LAST" = 1 ] && [ -n "$SPOOLWAY_EPIC" ]; then
      acli jira workitem transition --key "$SPOOLWAY_EPIC" --status "$status_review" --yes \
        || exit $?
    fi
    ;;
esac
