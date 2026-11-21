#!/usr/bin/env bash
# Renders the README's TUI screenshots from the demo project, the way the
# earlier ones were made: the screen is captured with its colours, `freeze`
# draws it as an SVG in JetBrains Mono on GitHub's dark background, and
# headless Chrome turns that SVG into a PNG at twice its size. freeze's own
# PNG writer crashes with a Go stack overflow on these screens, so freeze only
# writes the SVG.
#
# Usage:
#   screenshots.sh [DIR]             queue, routines, eval and plan. The tabs
#                                    come from a private tmux server the person
#                                    never sees. Run it after setup and before
#                                    the person queues the groups: the queue
#                                    tab is empty after.
#   screenshots.sh --plan [DIR]      only the plan page: plan-page.py fills the
#                                    shipped spoolway-plan template into the
#                                    demo's plans folder, and Chrome shoots its
#                                    top in the light theme.
#   screenshots.sh --dispatch [DIR]  the dispatch tab, read from the demo herdr
#                                    session's pane while its dispatcher runs
#                                    lanes. It only reads the pane, and refuses
#                                    when the pane is not on a running board.
#
# DIR defaults to the repository's docs/screenshots/readme/. Prints each PNG.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(git -C "$HERE" rev-parse --show-toplevel)"
DEMO="$HOME/demo/shop"
BG="#0d1117"
SOCK=spoolway-readme-shots

mode=tabs
case "${1:-}" in
  --dispatch) mode=dispatch; shift ;;
  --plan) mode=plan; shift ;;
esac
OUT="${1:-$REPO_ROOT/docs/screenshots/readme}"

[[ -d "$DEMO" ]] || { echo "no demo at $DEMO; run setup.sh first" >&2; exit 1; }
for c in freeze google-chrome python3 tmux; do
  command -v "$c" >/dev/null || { echo "$c is not installed" >&2; exit 1; }
done
mkdir -p "$OUT"
WORK=$(mktemp -d)
T() { tmux -L "$SOCK" "$@"; }
trap 'T kill-server 2>/dev/null || true; rm -rf "$WORK"' EXIT

# Strips colour codes, for matching a capture's text.
plain() { sed -e 's/\x1b\[[0-9;?]*[A-Za-z]//g' "$1"; }

# chrome_shot W H PNG URL: headless Chrome at twice the size.
chrome_shot() {
  rm -f "$3"
  google-chrome --headless=new --disable-gpu --no-sandbox --hide-scrollbars \
    --force-device-scale-factor=2 --default-background-color=00000000 \
    --window-size="$1,$2" --user-data-dir="$WORK/chrome" \
    --screenshot="$3" "$4" >/dev/null 2>&1 || true
  [[ -s "$3" ]] || { echo "could not render $3" >&2; exit 1; }
  echo "$3"
}

# plan_shot: the plan page, cut off below Context's folder tree.
plan_shot() {
  local plans page
  plans=$(spoolway -C "$DEMO" config path | awk '$1 == "plans:" {print $2}')
  page=$(python3 -I "$HERE/plan-page.py" \
    "$REPO_ROOT/assets/skills/claude/spoolway-plan/assets/template.html" \
    "$plans/$(date +%F)-cart.html")
  chrome_shot 1100 640 "$OUT/plan.png" "file://$page"
}

# render NAME: $WORK/NAME.ansi -> $OUT/NAME.png
render() {
  local name=$1 svg="$WORK/$1.svg" w h
  # Carriage returns and the blank rows above and below the screen go.
  python3 -I - "$WORK/$name.ansi" <<'PY'
import re, sys
path = sys.argv[1]
lines = open(path, encoding="utf-8", errors="replace").read().replace("\r", "").split("\n")
def blank(line):
    return not re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", line).strip()
while lines and blank(lines[0]):
    lines.pop(0)
while lines and blank(lines[-1]):
    lines.pop()
open(path, "w", encoding="utf-8").write("\n".join(lines) + "\n")
PY
  freeze --background "$BG" -o "$svg" < "$WORK/$name.ansi" >/dev/null
  read -r w h < <(python3 -I - "$svg" <<'PY'
import math, re, sys
m = re.search(r'<svg width="([0-9.]+)" height="([0-9.]+)"', open(sys.argv[1]).read(4096))
print(math.ceil(float(m[1])), math.ceil(float(m[2])))
PY
)
  chrome_shot "$w" "$h" "$OUT/$name.png" "file://$svg"
}

if [[ "$mode" == dispatch ]]; then
  h() { herdr --session demo "$@"; }
  ws=$(h workspace list | python3 -I -c '
import json, sys
ws = json.load(sys.stdin)["result"]["workspaces"]
print(ws[0]["workspace_id"] if ws else "")')
  [[ -n "$ws" ]] || { echo "the demo herdr session has no workspace; run open.sh" >&2; exit 1; }
  pane=$(h pane list --workspace "$ws" | python3 -I -c '
import json, sys
print(json.load(sys.stdin)["result"]["panes"][0]["pane_id"])')
  h pane read "$pane" --source visible --format ansi > "$WORK/dispatch.ansi"
  if ! plain "$WORK/dispatch.ansi" | grep -q "dispatcher running"; then
    echo "the demo pane is not on a running dispatch board; switch it to [1] DISPATCH" >&2
    exit 1
  fi
  render dispatch
  exit 0
fi

if [[ "$mode" == plan ]]; then
  plan_shot
  exit 0
fi

# wait_for TEXT [COUNT]: until the screen shows TEXT at least COUNT times.
wait_for() {
  for _ in $(seq 1 40); do
    (( $(T capture-pane -p -t shots | grep -c -F -- "$1") >= ${2:-1} )) && return 0
    sleep 0.5
  done
  echo "the screen never showed '$1'" >&2
  exit 1
}
# shot NAME: capture the screen once it has settled.
shot() {
  sleep 1
  T capture-pane -ep -t shots > "$WORK/$1.ansi"
  render "$1"
}

T kill-server 2>/dev/null || true
T new-session -d -s shots -x 140 -y 29 -c "$DEMO" \
  "TERM=xterm-256color COLORTERM=truecolor spoolway"
wait_for "[1] DISPATCH"

# The queue tab with both groups ticked and the cursor on cart.
T send-keys -t shots 2
wait_for "[groups]"
wait_for "[ ] cart"
T send-keys -t shots Space
T send-keys -t shots Down
T send-keys -t shots Space
wait_for "[x]" 2
shot queue

# The routines tab on nightly, whose details show its job.
T resize-window -t shots -x 130 -y 34
T send-keys -t shots 3
wait_for "job:"
shot routines

# The eval tab's lanes table, short enough to show only the table.
T resize-window -t shots -x 160 -y 15
T send-keys -t shots 5
wait_for "PIPELINE"
wait_for "TIME"
shot eval

T kill-server
plan_shot
