#!/usr/bin/env bash
# Runs the demo's dispatcher with nobody watching, for the dispatch screenshot
# when no one records a video. herdr runs as the `demo` session inside a
# private tmux server, bare `spoolway` runs in its pane, and this script
# queues both groups and starts dispatching, as the person would on camera.
# It returns once two lanes run and three steps have passed. Then run
# `screenshots.sh --dispatch`, and `teardown.sh`, which stops the `demo`
# session and so ends the tmux server too.
#
# Usage: headless-dispatch.sh
set -euo pipefail

SESSION=demo
REPO="$HOME/demo/shop"
[[ -d "$REPO" ]] || { echo "no demo at $REPO; run setup.sh first" >&2; exit 1; }

session_running() {
  herdr session list 2>/dev/null | awk -v s="$SESSION" '$1 == s && $2 == "running" {f=1} END {exit !f}'
}
if session_running; then
  echo "herdr session '$SESSION' is already running; run teardown.sh first" >&2
  exit 1
fi

# Without herdr's variables: a shell inside the person's own herdr carries
# them, and a herdr started with them never comes up.
env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_SOCKET_PATH -u HERDR_TAB_ID -u HERDR_WORKSPACE_ID \
  tmux -L spoolway-readme-herdr -f /dev/null new-session -d -s herdr -x 200 -y 48 -c "$REPO" \
  "zsh -ic 'herdr --session $SESSION'"
for _ in $(seq 1 40); do session_running && break; sleep 0.5; done
session_running || { echo "herdr session '$SESSION' did not start" >&2; exit 1; }

h() { herdr --session "$SESSION" "$@"; }
ws=""
for _ in $(seq 1 20); do
  ws=$(h workspace list 2>/dev/null | python3 -I -c '
import json, sys
ws = json.load(sys.stdin)["result"]["workspaces"]
print(ws[0]["workspace_id"] if ws else "")' 2>/dev/null || true)
  [[ -n "$ws" ]] && break
  sleep 0.5
done
[[ -n "$ws" ]] || { echo "the '$SESSION' session has no workspace" >&2; exit 1; }
pane=$(h pane list --workspace "$ws" | python3 -I -c '
import json, sys
print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')
h workspace rename "$ws" shop >/dev/null
h pane run "$pane" spoolway >/dev/null

screen() { h pane read "$pane" --source visible --format text; }
key() { h pane send-keys "$pane" "$@" >/dev/null; sleep 0.6; }
# wait_until SECONDS CHECK: until the screen passes CHECK, a grep over stdin.
wait_until() {
  local limit=$1; shift
  for _ in $(seq 1 "$limit"); do screen | "$@" && return 0; sleep 1; done
  echo "the board never got there: $*" >&2
  exit 1
}

wait_until 20 grep -q -F "[1] DISPATCH"
# Queue both groups on the queue tab, then start dispatching and confirm.
key 2
wait_until 10 grep -q -F "[ ] cart"
key space; key down; key space
wait_until 10 grep -q -F "> [x] cart"
key enter
wait_until 10 grep -q "nothing to queue"
key 1; key enter
wait_until 10 grep -q "before dispatching"
key enter
wait_until 360 awk '/● running/ {r++} /passed/ {p++} END {exit !(r >= 2 && p >= 3)}'
echo "$SESSION $pane"
