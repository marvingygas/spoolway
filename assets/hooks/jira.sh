#!/bin/sh
# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Called with `open` as the group is queued, `fetch` when
# someone runs `spoolway issue show`, then on queued, blocked, paused and
# done.
#
# Every `acli` command below was checked against acli 1.3.30-stable. Three
# things need a site to confirm, and each is a one-line edit if yours
# differs: that `workitem create --json` names the new key `.key`, that your
# project spells its link type `Blocks` and its epic status `Done`, and — for
# `fetch`, below — that `workitem view --json` names the summary, status and
# label fields the way this reads them; run it once by hand against a real
# key and adjust the `jq` filter to match if it does not.
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

# Hangs $2 (an issue this hook just created, by its own key) as a Jira link
# under $1, doing nothing at all unless $1 is a `.../browse/<key>` URL for
# this same $project. $1 is SPOOLWAY_SOURCE most of the time, and spoolway
# never parses that field — most of the time it names a plan page's path,
# not an issue, and this is what lets it fall straight through untouched.
# "Relates" is this codebase's own guess at a link type every site ships;
# the one edit to make if yours differs.
hang_under() {
  child=$2
  [ -z "$child" ] && return 0
  case "$1" in
    */browse/"$project"-[0-9]*) ;;
    *) return 0 ;;
  esac
  acli jira workitem link create --out "${1##*/}" --in "$child" --type Relates --yes
}

if [ "$SPOOLWAY_EVENT" = fetch ]; then
  fields=$(acli jira workitem view --key "$SPOOLWAY_REF" --json) && [ -n "$fields" ] || {
    echo "jira.sh: acli returned nothing for \"$SPOOLWAY_REF\" — does the key exist?" >&2
    exit 1
  }
  comments=$(acli jira workitem comment list --key "$SPOOLWAY_REF" --json 2>/dev/null)
  jq -n --argjson f "$fields" --argjson c "${comments:-[]}" '{
    ref: $f.key,
    url: (($f.self | capture("(?<site>https://[^/]+)").site) + "/browse/" + $f.key),
    title: $f.summary,
    state: ($f.status.name // $f.status // "" | ascii_downcase),
    labels: ($f.labels // []),
    body: ($f.description // ""),
    comments: [$c[]? | {author: (.author.name // .author // ""), body: (.body // "")}]
  }' > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT" = open ]; then
  epic=$SPOOLWAY_EPIC                       # set when the group already names one
  if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
    epic=$(acli jira workitem create --project "$project" --type Epic \
             --summary "$SPOOLWAY_GROUP" \
             --description-file "$SPOOLWAY_EPIC_BODY" --json | jq -r '.key // empty')
    if [ -z "$epic" ]; then
      echo "jira.sh: acli/jq returned no epic key — is jq installed, and did \`workitem create\` succeed?" >&2
      exit 1
    fi
    hang_under "$SPOOLWAY_SOURCE" "$epic"
  fi
  ticket=$(acli jira workitem create --project "$project" --type Story \
             --summary "$SPOOLWAY_TITLE" \
             --description-file "$SPOOLWAY_TICKET_BODY" \
             ${epic:+--parent "$epic"} --json | jq -r '.key // empty')
  if [ -z "$ticket" ]; then
    echo "jira.sh: acli/jq returned no ticket key — is jq installed, and did \`workitem create\` succeed?" >&2
    exit 1
  fi
  [ -z "$epic" ] && hang_under "$SPOOLWAY_SOURCE" "$ticket"  # a group of one has no epic to hang under
  for dep in $SPOOLWAY_DEPENDS_TICKETS; do   # Jira has real issue links
    acli jira workitem link create --out "$dep" --in "$ticket" \
      --type Blocks --yes                    # --type takes the outward wording
  done
  { echo "epic=$epic"; echo "ticket=$ticket"; } > "$SPOOLWAY_OUT"
  exit 0
fi

if [ "$SPOOLWAY_EVENT$SPOOLWAY_GROUP_LAST" = done1 ] && [ -n "$SPOOLWAY_EPIC" ]; then
  acli jira workitem transition --key "$SPOOLWAY_EPIC" --status Done --yes
fi

case "$SPOOLWAY_EVENT" in blocked|paused) ;; *) exit 0 ;; esac

acli jira workitem comment create --key "$SPOOLWAY_TICKET" --body \
  "spoolway - $SPOOLWAY_TASK is $SPOOLWAY_EVENT at $SPOOLWAY_FROM"
