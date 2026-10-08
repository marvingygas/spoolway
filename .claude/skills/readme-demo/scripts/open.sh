#!/usr/bin/env bash
# Opens the demo in its own herdr session, `demo`, in a new Ubuntu window, so
# none of the person's own workspaces show in the recording. Names the
# session's workspace `shop` and starts bare `spoolway` in it. The dispatcher
# is not started: the person recording presses enter on the dispatch tab.
set -euo pipefail

SESSION=demo
REPO="$HOME/demo/shop"
[[ -d "$REPO" ]] || { echo "no demo at $REPO; run setup.sh first" >&2; exit 1; }
command -v cmd.exe >/dev/null || { echo "cmd.exe not found: this needs WSL on Windows" >&2; exit 1; }

session_running() {
  herdr session list 2>/dev/null | awk -v s="$SESSION" '$1 == s && $2 == "running" {f=1} END {exit !f}'
}
if session_running; then
  echo "herdr session '$SESSION' is already running; run teardown.sh first" >&2
  exit 1
fi

# Launched the way the person's pinned Ubuntu taskbar icon does it: plain
# wsl.exe started by Windows, so it opens in their default terminal with their
# usual look. Not `wt.exe -p Ubuntu`: that profile has a different font.
# An interactive zsh, because herdr's PATH entry lives in ~/.zshrc.
# Detached: cmd.exe does not exit after `start`, and waiting on it hangs this
# script while the window itself is already up.
(cd /mnt/c && cmd.exe /c start "" "C:\Program Files\WSL\wsl.exe" -d "$WSL_DISTRO_NAME" \
  --cd "$REPO" -- /usr/bin/zsh -ic "herdr --session $SESSION" </dev/null >/dev/null 2>&1 &)

for _ in $(seq 1 40); do session_running && break; sleep 0.5; done
session_running || { echo "herdr session '$SESSION' did not start" >&2; exit 1; }

h() { herdr --session "$SESSION" "$@"; }
ws=""
for _ in $(seq 1 20); do
  ws=$(h workspace list 2>/dev/null | python3 -c '
import json, sys
ws = json.load(sys.stdin)["result"]["workspaces"]
print(ws[0]["workspace_id"] if ws else "")' 2>/dev/null || true)
  [[ -n "$ws" ]] && break
  sleep 0.5
done
[[ -n "$ws" ]] || { echo "the '$SESSION' session has no workspace" >&2; exit 1; }

pane=$(h pane list --workspace "$ws" | python3 -c '
import json, sys
print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')
h workspace rename "$ws" shop >/dev/null
h pane run "$pane" spoolway >/dev/null
echo "$SESSION $pane"
