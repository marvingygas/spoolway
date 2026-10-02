#!/usr/bin/env bash
# A minimal `acli` double for `jira.sh`'s `open` branch — the only event an
# e2e suite can exercise against a stub at all, since `started`, `blocked`,
# `paused` and `done` depend on this project's own `status_*`/`link_*` names
# actually meaning something to whatever answers `acli`, and the hook's own
# live proof against a real Jira site is what actually proves those. `open`
# is different: it only ever creates, asks for a key back as JSON, and
# optionally reads labels back — nothing in it depends on a real workflow.
#
# Work items are flat files under $ACLI_STUB_DIR, one per key: `project=`,
# `type=`, `summary=` and `labels=` lines, plus `<key>.description.json`
# holding the exact bytes `--description-file` named — the ADF `jira.sh`
# itself built, not a copy of this stub's own idea of it. `<key>.parent`
# holds `--parent`'s value when one was given.
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
          *) exit 0 ;;
        esac
        ;;
      *) exit 0 ;;
    esac
    ;;
  *) exit 0 ;;
esac
