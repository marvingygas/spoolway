#!/usr/bin/env bash
# A minimal `acli` double for `jira.sh`. `open` is the one branch that only
# ever creates, asks for a key back as JSON, and optionally reads labels
# back, so it is the only one whose *result* (a Story and Sub-task, with the
# right parent, labels and ADF body) an e2e suite can actually check against
# a stub. `started`, `blocked` and `paused` lean on this project's own
# `status_*`/`link_*` names meaning something real on the far end, which
# only a live Jira site proves — see this task's own live proof against KAN
# for that half. `done`'s own `acli` calls (the review comment, the Review
# transition) are the same shape: this double records that they were made
# and with what, which is enough to prove `done` never comments on the pull
# request it names, without proving the transition itself lands anywhere.
#
# Work items are flat files under $ACLI_STUB_DIR, one per key: `project=`,
# `type=`, `summary=` and `labels=` lines, plus `<key>.description.json`
# holding the exact bytes `--description-file` named — the ADF `jira.sh`
# itself built, not a copy of this stub's own idea of it. `<key>.parent`
# holds `--parent`'s value when one was given. `<key>.comment.json` holds
# the exact bytes `workitem comment create --body-file` named, and
# `<key>.status` the last status a `workitem transition --status` named.
set -euo pipefail

DIR=${ACLI_STUB_DIR:?ACLI_STUB_DIR must name where work items live}
mkdir -p "$DIR"

next_key() {
  local prefix=${ACLI_STUB_PROJECT_PREFIX:-KAN} n=0 f b num
  for f in "$DIR/$prefix"-[0-9]*; do
    [ -e "$f" ] || continue
    b=$(basename "$f")
    case "$b" in *.*) continue ;; esac
    num=${b#"$prefix"-}
    [ "$num" -gt "$n" ] && n=$num
  done
  echo $((n + 1))
}

case "${1:-}" in
  # `spoolway doctor` and the submit-time version gate both read this back
  # against jira.sh's own `# spoolway-requires: acli >= 1.3.39` line —
  # answered above that floor, so a gated run against this double reads the
  # same as one against the real, tested `acli`.
  --version) echo "acli version 1.3.40-stable"; exit 0 ;;
  jira)
    case "${2:-}" in
      auth)
        # `site_host` in jira.sh reads only the `  Site: …` line back —
        # real `acli jira auth status` prints several more, which this
        # double does not bother reproducing.
        echo "✓ Authenticated"
        echo "  Site: ${ACLI_STUB_SITE:-stub.atlassian.net}"
        ;;
      workitem)
        case "${3:-}" in
          create)
            shift 3
            project="" type="" summary="" descfile="" parent="" labels=""
            while [ $# -gt 0 ]; do
              case "$1" in
                --project) project=$2; shift 2 ;;
                --type) type=$2; shift 2 ;;
                --summary) summary=$2; shift 2 ;;
                --description-file) descfile=$2; shift 2 ;;
                --parent) parent=$2; shift 2 ;;
                --label) labels="$labels${labels:+,}$2"; shift 2 ;;
                --json) shift ;;
                *) shift ;;
              esac
            done
            n=$(next_key)
            key="${ACLI_STUB_PROJECT_PREFIX:-KAN}-$n"
            {
              echo "project=$project"
              echo "type=$type"
              echo "summary=$summary"
              echo "labels=$labels"
            } > "$DIR/$key"
            cp "$descfile" "$DIR/$key.description.json"
            [ -n "$parent" ] && echo "$parent" > "$DIR/$key.parent"
            printf '{"key":"%s"}\n' "$key"
            ;;
          view)
            # Only the one shape `union_labels` needs: `--fields labels
            # --json`, read back as `.fields.labels`.
            ref=$4
            printf '{"fields":{"labels":[%s]}}\n' \
              "$(sed -n "s/^labels=//p" "$DIR/$ref" 2>/dev/null \
                 | tr ',' '\n' | sed '/^$/d' | sed 's/.*/"&"/' | paste -sd, -)"
            ;;
          edit)
            shift 3
            ref="" labels=""
            while [ $# -gt 0 ]; do
              case "$1" in
                --key) ref=$2; shift 2 ;;
                --labels) labels=$2; shift 2 ;;
                --yes) shift ;;
                *) shift ;;
              esac
            done
            [ -n "$ref" ] || exit 0
            sed -i "s/^labels=.*/labels=$labels/" "$DIR/$ref" 2>/dev/null || true
            ;;
          link)
            # Best-effort in the real hook too (`|| true` at every call
            # site) — nothing reads this double's answer either way.
            exit 0
            ;;
          comment)
            case "${4:-}" in
              create)
                shift 4
                ref="" bodyfile=""
                while [ $# -gt 0 ]; do
                  case "$1" in
                    --key) ref=$2; shift 2 ;;
                    --body-file) bodyfile=$2; shift 2 ;;
                    --body) shift 2 ;;
                    *) shift ;;
                  esac
                done
                [ -n "$ref" ] && [ -n "$bodyfile" ] && cp "$bodyfile" "$DIR/$ref.comment.json"
                echo "✓ Comment on work item $ref has been successfully added"
                ;;
              *) exit 0 ;;
            esac
            ;;
          transition)
            shift 3
            ref="" status=""
            while [ $# -gt 0 ]; do
              case "$1" in
                --key) ref=$2; shift 2 ;;
                --status) status=$2; shift 2 ;;
                --yes) shift ;;
                *) shift ;;
              esac
            done
            [ -n "$ref" ] && printf '%s' "$status" > "$DIR/$ref.status"
            echo "✓ Work item $ref has been successfully transitioned to $status"
            ;;
          *) exit 0 ;;
        esac
        ;;
      *) exit 0 ;;
    esac
    ;;
  *) exit 0 ;;
esac
