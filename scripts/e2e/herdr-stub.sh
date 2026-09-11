#!/usr/bin/env bash
# A minimal `herdr` double, for the one case in
# `scripts/e2e/suites/commands.sh` that has to watch a *pane* receive its
# environment.
#
# Why a double rather than a real server. tmux hands a suite an instance of
# its own for the asking — `tmux -S <socket>` is a private server nobody else
# can see — and the paned-command-step case above uses exactly that. herdr has
# no equivalent: pointing its CLI at another socket path still reaches the one
# persistent session on the machine, so a suite that drove it would be typing
# into whatever a person, or another lane, happens to have open. That is not a
# test anybody can run.
#
# What it is faithful about, which is the whole point:
#
#   * a pane is a **long-lived shell**, not a command runner. Each pane here is
#     a real `sh` reading from a fifo, so two `pane run` calls are two lines
#     typed into the same shell — and a `.` on the first line is what puts a
#     variable in reach of the second.
#   * a pane's shell starts from the **server's** environment, never the
#     dispatcher's. Every pane here is started under `env -i`, so a variable
#     that reaches one got there because spoolway carried it, not because the
#     harness leaked it. Without that the case would pass on any code at all.
#
# What it is not: herdr. It agrees with `src/mux.rs` about JSON shapes by
# construction, so it proves nothing about them — those are unit-tested in
# `src/mux.rs` against captured payloads. It is here for the handover, and the
# transcript it keeps is the other half of that: every string spoolway typed
# into a pane, so a case can assert nothing long was ever typed at all.
#
# State lives under $HERDR_STUB_STATE:
#
#   seq            the id counter
#   workspaces     workspace_id \t label \t checkout_path (empty for none)
#   tabs           tab_id \t workspace_id \t label
#   panes          pane_id \t tab_id \t workspace_id \t label
#   typed/<n>      one file per `pane run`, holding exactly what was typed
#   typed.index    pane_id \t bytes \t typed/<n>, one line per `pane run`
#   p<n>.in        the fifo that pane's shell reads its lines from
#   p<n>.out       everything that pane's shell has printed
set -uo pipefail

STATE=${HERDR_STUB_STATE:?HERDR_STUB_STATE must name where this double keeps its state}
mkdir -p "$STATE" "$STATE/typed"
: >>"$STATE/workspaces"; : >>"$STATE/tabs"; : >>"$STATE/panes"; : >>"$STATE/typed.index"

# The environment a pane's shell starts with — a stand-in for the herdr
# server's own, and deliberately nothing like the dispatcher's. Overridable so
# a suite whose `run:` line needs a tool can say where it is.
PANE_PATH=${HERDR_STUB_PANE_PATH:-/usr/local/bin:/usr/bin:/bin}

# ------------------------------------------------------------------ plumbing
fail() { # code message
  printf '{"error":{"code":"%s","message":"%s"}}\n' "$1" "$2" >&2
  exit 1
}

# A JSON string, with the two characters that can appear in a path escaped.
jstr() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  printf '"%s"' "$s"
}

# The next id, counted under a lock: the dispatcher and its `--watch` board
# can both be in here at once.
next_id() {
  local n
  exec 9>"$STATE/seq.lock"
  flock 9
  n=$(cat "$STATE/seq" 2>/dev/null || echo 0)
  n=$((n + 1))
  echo "$n" >"$STATE/seq"
  flock -u 9
  echo "$n"
}

# Flags off the argv, in whatever order they came. `--no-focus` and `--force`
# take no value and are simply ignored: this double has one focus and nothing
# to force.
declare -A FLAG=()
POS=()
parse_flags() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --no-focus|--force) shift ;;
      --*) FLAG[${1#--}]=${2:-}; shift 2 ;;
      *) POS+=("$1"); shift ;;
    esac
  done
}

field() { awk -F'\t' -v k="$1" -v n="$2" '$1==k {print $n; exit}' "$3"; }

# ------------------------------------------------------------------- panes
# A pane is a real shell reading a fifo. Two writers keep that fifo alive: a
# `holder` that opens it and sleeps, so the shell never reads EOF between two
# `pane run` calls, and each `pane run` itself.
open_pane() { # pane_id cwd
  local pane=$1 cwd=$2 n=${1##*:p}
  local fifo="$STATE/p$n.in"
  rm -f "$fifo"
  mkfifo "$fifo"
  : >"$STATE/p$n.out"

  # The holder ends when the fifo does, so a pane nobody closed cannot outlive
  # the suite that made it: `close_pane` removes the fifo, and so does the
  # `shutdown` verb below.
  setsid sh -c 'exec 3>"$1"; while [ -p "$1" ]; do sleep 2; done' _ "$fifo" \
    >/dev/null 2>&1 &
  echo $! >"$STATE/p$n.holder"

  # `env -i`: the pane's shell inherits nothing from the dispatcher that
  # invoked this double. See the header — this is the load-bearing line.
  setsid env -i HOME="${HOME:-/tmp}" PATH="$PANE_PATH" TERM=dumb \
    sh -c 'cd "$1" || exit 1; exec sh' _ "$cwd" \
    <"$fifo" >>"$STATE/p$n.out" 2>&1 &
  echo $! >"$STATE/p$n.shell"
}

close_pane() { # pane_id
  local n=${1##*:p}
  for role in holder shell; do
    local pid
    pid=$(cat "$STATE/p$n.$role" 2>/dev/null || true)
    [ -n "$pid" ] && kill -- "-$pid" 2>/dev/null
    rm -f "$STATE/p$n.$role"
  done
  rm -f "$STATE/p$n.in"
  grep -v "^$1	" "$STATE/panes" >"$STATE/panes.tmp" 2>/dev/null || true
  mv "$STATE/panes.tmp" "$STATE/panes"
}

# workspace_id tab_id label cwd -> pane_id, recorded and running
make_pane() {
  local ws=$1 tab=$2 label=$3 cwd=$4 n pane
  n=$(next_id)
  pane="$ws:p$n"
  printf '%s\t%s\t%s\t%s\n' "$pane" "$tab" "$ws" "$label" >>"$STATE/panes"
  open_pane "$pane" "$cwd"
  echo "$pane"
}

# label checkout_path -> workspace_id, with a tab and a root pane
make_workspace() {
  local label=$1 checkout=$2 cwd=$3 n ws tab pane
  n=$(next_id)
  ws="w$n"
  printf '%s\t%s\t%s\n' "$ws" "$label" "$checkout" >>"$STATE/workspaces"
  tab="$ws:t1"
  printf '%s\t%s\t%s\n' "$tab" "$ws" "$label" >>"$STATE/tabs"
  pane=$(make_pane "$ws" "$tab" "$label" "$cwd")
  printf '{"result":{"workspace":{"workspace_id":%s},"root_pane":{"pane_id":%s,"tab_id":%s}}}\n' \
    "$(jstr "$ws")" "$(jstr "$pane")" "$(jstr "$tab")"
}

# ------------------------------------------------------------------ commands
DOMAIN=${1:-}; shift || true
VERB=${1:-}; shift || true
parse_flags "$@"

case "$DOMAIN $VERB" in

  "agent list")
    # No agent is ever started through this double, and nothing here needs
    # one: a command step's pane runs a shell script, which herdr itself
    # reports as agentless. An empty list is the honest answer, and it is also
    # what makes every pane a valid split target in `choose_split`.
    echo '{"result":{"agents":[]}}'
    ;;

  "pane current")
    # The dispatcher is not running in a pane of this double. herdr answers a
    # usage error here, and `Herdr::own_pane` reads any failure as "no pane".
    fail no_current_pane "not running in a herdr pane"
    ;;

  "workspace list")
    { printf '{"result":{"workspaces":['
      first=1
      while IFS=$'\t' read -r ws label checkout; do
        [ -n "$ws" ] || continue
        [ $first -eq 1 ] || printf ','
        first=0
        printf '{"workspace_id":%s,"label":%s,"worktree":' "$(jstr "$ws")" "$(jstr "$label")"
        if [ -n "$checkout" ]; then
          printf '{"checkout_path":%s}' "$(jstr "$checkout")"
        else
          printf 'null'
        fi
        printf '}'
      done <"$STATE/workspaces"
      printf ']}}\n'; }
    ;;

  "workspace create")
    # No checkout bound: this is the call herdr answers with `worktree: null`,
    # and `Herdr::dispatch_workspace` leans on that absence.
    make_workspace "${FLAG[label]:-}" "" "${FLAG[cwd]:-$PWD}"
    ;;

  "worktree open")
    # `--cwd` is the repository herdr resolves the row under, `--path` the
    # checkout the workspace is opened on. Refused when `--cwd` is itself a
    # linked worktree, which is the real herdr behaviour this task's `--cwd`
    # fix exists for — so the double refuses it too, and a regression there
    # fails here rather than passing quietly.
    root=${FLAG[cwd]:-$PWD}
    if [ -f "$root/.git" ]; then
      fail linked_worktree_source "--cwd is a linked worktree, not a main checkout"
    fi
    make_workspace "${FLAG[label]:-}" "${FLAG[path]:-}" "${FLAG[path]:-$PWD}"
    ;;

  "workspace close")
    ws=${POS[0]:-${FLAG[workspace]:-}}
    # Over a snapshot: `close_pane` rewrites `panes` underneath us.
    cp "$STATE/panes" "$STATE/panes.snap"
    while IFS=$'\t' read -r pane _tab pws _label; do
      [ "$pws" = "$ws" ] && close_pane "$pane"
    done <"$STATE/panes.snap"
    grep -v "^$ws	" "$STATE/workspaces" >"$STATE/workspaces.tmp" 2>/dev/null || true
    mv "$STATE/workspaces.tmp" "$STATE/workspaces"
    echo '{"result":{}}'
    ;;

  "worktree remove")
    ws=${FLAG[workspace]:-}
    checkout=$(field "$ws" 3 "$STATE/workspaces")
    [ -n "$checkout" ] || fail not_linked_worktree "workspace holds no worktree"
    echo '{"result":{}}'
    ;;

  "tab list")
    { printf '{"result":{"tabs":['
      first=1
      while IFS=$'\t' read -r tab ws label; do
        [ "$ws" = "${FLAG[workspace]:-}" ] || continue
        [ $first -eq 1 ] || printf ','
        first=0
        printf '{"tab_id":%s,"label":%s}' "$(jstr "$tab")" "$(jstr "$label")"
      done <"$STATE/tabs"
      printf ']}}\n'; }
    ;;

  "tab create")
    ws=${FLAG[workspace]:?}
    n=$(next_id)
    tab="$ws:t$n"
    printf '%s\t%s\t%s\n' "$tab" "$ws" "${FLAG[label]:-}" >>"$STATE/tabs"
    pane=$(make_pane "$ws" "$tab" "${FLAG[label]:-}" "${FLAG[cwd]:-$PWD}")
    printf '{"result":{"tab":{"tab_id":%s},"root_pane":{"pane_id":%s,"tab_id":%s}}}\n' \
      "$(jstr "$tab")" "$(jstr "$pane")" "$(jstr "$tab")"
    ;;

  "tab close")
    tab=${POS[0]:-}
    cp "$STATE/panes" "$STATE/panes.snap"
    while IFS=$'\t' read -r pane ptab _ws _label; do
      [ "$ptab" = "$tab" ] && close_pane "$pane"
    done <"$STATE/panes.snap"
    grep -v "^$tab	" "$STATE/tabs" >"$STATE/tabs.tmp" 2>/dev/null || true
    mv "$STATE/tabs.tmp" "$STATE/tabs"
    echo '{"result":{}}'
    ;;

  "pane list")
    { printf '{"result":{"panes":['
      first=1
      while IFS=$'\t' read -r pane tab _ws _label; do
        [ -n "$pane" ] || continue
        [ $first -eq 1 ] || printf ','
        first=0
        printf '{"pane_id":%s,"tab_id":%s}' "$(jstr "$pane")" "$(jstr "$tab")"
      done <"$STATE/panes"
      printf ']}}\n'; }
    ;;

  "pane layout")
    # Addressed by pane, and the layout answered is that pane's whole tab —
    # herdr's own shape, and the one `choose_split` reads. Every pane is the
    # same size here: which one gets split is not what this double is for.
    tab=$(field "${FLAG[pane]:-}" 2 "$STATE/panes")
    { printf '{"result":{"layout":{"panes":['
      first=1
      while IFS=$'\t' read -r pane ptab _ws _label; do
        [ "$ptab" = "$tab" ] || continue
        [ $first -eq 1 ] || printf ','
        first=0
        printf '{"pane_id":%s,"rect":{"width":200,"height":60}}' "$(jstr "$pane")"
      done <"$STATE/panes"
      printf ']}}}\n'; }
    ;;

  "pane split")
    target=${FLAG[pane]:?}
    tab=$(field "$target" 2 "$STATE/panes")
    ws=$(field "$target" 3 "$STATE/panes")
    [ -n "$tab" ] || fail no_such_pane "no pane $target"
    pane=$(make_pane "$ws" "$tab" "" "${FLAG[cwd]:-$PWD}")
    printf '{"result":{"pane":{"pane_id":%s,"tab_id":%s}}}\n' \
      "$(jstr "$pane")" "$(jstr "$tab")"
    ;;

  "pane rename")
    echo '{"result":{}}'
    ;;

  "pane run")
    pane=${POS[0]:?}
    cmd=${POS[1]:-}
    n=${pane##*:p}
    fifo="$STATE/p$n.in"
    [ -p "$fifo" ] || fail no_such_pane "no pane $pane"
    # The transcript. Every string spoolway typed into a pane, kept whole and
    # measured, so a case can assert what got typed rather than only what came
    # out the far end.
    slot=$(next_id)
    printf '%s' "$cmd" >"$STATE/typed/$slot"
    printf '%s\t%s\t%s\n' "$pane" "$(printf '%s' "$cmd" | wc -c | tr -d ' ')" \
      "typed/$slot" >>"$STATE/typed.index"
    printf '%s\n' "$cmd" >"$fifo"
    echo '{"result":{}}'
    ;;

  "pane close")
    close_pane "${POS[0]:?}"
    echo '{"result":{}}'
    ;;

  "shutdown state")
    # Not a herdr verb: the suite's own teardown. A run that ends with a
    # workspace still open — a task that stopped short, a failed assertion —
    # would otherwise leave a shell and its fifo holder behind, and a test
    # harness that leaks processes is its own bug.
    cp "$STATE/panes" "$STATE/panes.snap"
    while IFS=$'\t' read -r pane _tab _ws _label; do
      [ -n "$pane" ] && close_pane "$pane"
    done <"$STATE/panes.snap"
    echo '{"result":{}}'
    ;;

  *)
    fail unsupported "this double does not implement \`herdr $DOMAIN $VERB\`"
    ;;
esac
