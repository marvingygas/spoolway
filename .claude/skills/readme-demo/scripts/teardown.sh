#!/usr/bin/env bash
# Removes the demo completely: the herdr workspaces that show it, every
# process still running in it, the checkout, the project's home, its log, and
# the Claude and pi session files its lanes wrote.
#
# Usage: teardown.sh [--dry-run]
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"

DRY=0
[[ "${1:-}" == "--dry-run" ]] && DRY=1

DEMO="$HOME/demo"
REPO="$DEMO/shop"

# The project's home is named after the id `init` stamped into the checkout.
# When the checkout is already gone, find the home by the checkout it records.
HOMES=()
if [[ -f "$REPO/.git/spoolway-id" ]]; then
  HOMES+=("$HOME/.spoolway/shop-$(cat "$REPO/.git/spoolway-id")")
fi
for d in "$HOME"/.spoolway/shop-*/; do
  d=${d%/}
  grep -qs "$REPO" "$d/project.toml" && HOMES+=("$d")
done
mapfile -t HOMES < <(printf '%s\n' "${HOMES[@]}" | sort -u | sed '/^$/d')

ROOTS=("$REPO" "${HOMES[@]}")
under_roots() { # path
  local p=$1 r
  for r in "${ROOTS[@]}"; do [[ "$p" == "$r" || "$p" == "$r"/* ]] && return 0; done
  return 1
}

run() { if ((DRY)); then echo "  would: $*"; else "$@"; fi; }

# 1. herdr workspaces whose panes sit in the demo: the screen's own and every
#    lane's. Matched by directory, never by label: the real spoolway project's
#    lanes carry the same `spoolway/<task>` labels.
if command -v herdr >/dev/null && [[ "${HERDR_ENV:-}" == 1 ]]; then
  echo "herdr workspaces:"
  herdr workspace list 2>/dev/null | python3 -c '
import json, sys
for w in json.load(sys.stdin)["result"]["workspaces"]:
    print(w["workspace_id"], w.get("label") or "")
' | while read -r wid label; do
    herdr pane list --workspace "$wid" 2>/dev/null | python3 -c '
import json, sys
for p in json.load(sys.stdin)["result"]["panes"]:
    print(p.get("cwd") or ""); print(p.get("foreground_cwd") or "")
' | while read -r cwd; do
      if [[ -n "$cwd" ]] && under_roots "$cwd"; then echo "$wid $label"; break; fi
    done
  done | sort -u | while read -r wid label; do
    ((DRY)) || echo "  closing $wid ($label)"
    run herdr workspace close "$wid" >/dev/null
  done
fi

# The demo's own herdr session, opened by open.sh, and everything in it.
if command -v herdr >/dev/null && herdr session list 2>/dev/null | awk '$1 == "demo" {f=1} END {exit !f}'; then
  ((DRY)) || echo "  stopping and deleting herdr session demo"
  run herdr session stop demo >/dev/null 2>&1
  run herdr session delete demo >/dev/null 2>&1
fi

# 2. Anything still running in the demo: the screen, the dispatcher, lanes.
((DRY)) || sleep 1
echo "processes:"
for proc in /proc/[0-9]*; do
  pid=${proc#/proc/}
  [[ "$pid" == "$$" ]] && continue
  cwd=$(readlink "$proc/cwd" 2>/dev/null) || continue
  if under_roots "$cwd"; then
    ((DRY)) || echo "  stopping $pid $(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null | cut -c1-80)"
    run kill -TERM "$pid" 2>/dev/null
  fi
done
((DRY)) || sleep 2

# 3. Files.
shopt -s nullglob
targets=("$DEMO")
for h in "${HOMES[@]}"; do
  name=$(basename "$h")
  targets+=("$h" "$HOME/.spoolway/logs/$name.log")
  targets+=("$HOME"/.claude/projects/-home-*--spoolway-"$name"*)
  targets+=("$HOME"/.pi/agent/sessions/--home-*-.spoolway-"$name"*)
done
targets+=("$HOME"/.claude/projects/-home-*-demo-shop*)
targets+=("$HOME"/.pi/agent/sessions/--home-*-demo-shop*)

echo "files:"
for t in "${targets[@]}"; do
  [[ -e "$t" ]] || continue
  ((DRY)) || echo "  removing $t"
  run rm -rf -- "$t"
done

# The trust entry setup.sh added to ~/.claude.json.
if ((DRY)); then
  echo "  would: drop $REPO from ~/.claude.json"
else
  echo "  dropping $REPO from ~/.claude.json"
  python3 "$HERE/claude-trust.py" remove "$REPO"
fi

# 4. Proof nothing is left.
((DRY)) && exit 0
left=0
for t in "${targets[@]}"; do [[ -e "$t" ]] && { echo "still there: $t"; left=1; }; done
for proc in /proc/[0-9]*; do
  cwd=$(readlink "$proc/cwd" 2>/dev/null) || continue
  under_roots "$cwd" && { echo "still running: ${proc#/proc/}"; left=1; }
done
((left)) && exit 1
echo "demo removed"
