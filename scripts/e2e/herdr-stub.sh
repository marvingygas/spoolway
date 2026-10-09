#!/usr/bin/env bash
# A minimal `herdr` double, for the one case in
# `scripts/e2e/suites/command-steps.sh` that has to watch a *pane* receive its
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
# into a pane, so a case can assert nothing long was ever typed at all. A
# `pane run` over 512 bytes is refused with `line_too_long`, as a Mac's pane
# would garble it.
#
# An agent is a registration, not a process. `agent start` records that a
# named session sits in a pane, and `agent list` reports it idle for as long
# as the pane stands — which is what a finished agent that herdr has not been
# asked to close looks like. Nothing runs: a suite that wants a lane to report
# runs `spoolway report` itself, with the lane's `SPOOLWAY_TASK` and
# `SPOOLWAY_STEP`. That is enough to tell whether a pane outlives its step.
#
# State lives under $HERDR_STUB_STATE:
#
#   seq            the id counter
#   workspaces     workspace_id \t label \t checkout_path (empty for none)
#   tabs           tab_id \t workspace_id \t label
#   panes          pane_id \t tab_id \t workspace_id \t label
#   agents         name \t pane_id \t kind, one line per `agent start`
#   typed/<n>      one file per `pane run`, holding exactly what was typed
#   typed.index    pane_id \t bytes \t typed/<n>, one line per `pane run`
#   p<n>.cwd       the directory that pane's shell was started in
#   p<n>.in        the fifo that pane's shell reads its lines from
#   p<n>.out       everything that pane's shell has printed
set -uo pipefail

STATE=${HERDR_STUB_STATE:?HERDR_STUB_STATE must name where this double keeps its state}
mkdir -p "$STATE" "$STATE/typed"
: >>"$STATE/workspaces"; : >>"$STATE/tabs"; : >>"$STATE/panes"; : >>"$STATE/typed.index"
: >>"$STATE/agents"

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

# The next id, counted under a lock: the dispatcher and a suite's own
# foreground `spoolway` calls can both be in here at once.
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
# to force. A bare `--` ends the flags, and what follows is the agent's own
# argv, kept in REST and read by nothing here.
declare -A FLAG=()
POS=()
REST=()
parse_flags() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --) shift; REST=("$@"); return ;;
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
  printf '%s' "$cwd" >"$STATE/p$n.cwd"
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
  rm -f "$STATE/p$n.in" "$STATE/p$n.cwd"
  grep -v "^$1	" "$STATE/panes" >"$STATE/panes.tmp" 2>/dev/null || true
  mv "$STATE/panes.tmp" "$STATE/panes"
  # The agent in the pane goes with it, as it does in herdr.
  awk -F'\t' -v p="$1" '$2!=p' "$STATE/agents" >"$STATE/agents.tmp"
  mv "$STATE/agents.tmp" "$STATE/agents"
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
    # Every agent `agent start` registered whose pane still stands, idle: a
    # session that has finished its step and has not been closed. A pane that
    # holds a command step's shell script is reported as agentless, as herdr
    # does, because only `agent start` ever writes a row here.
    { printf '{"result":{"agents":['
      first=1
      while IFS=$'\t' read -r name pane kind; do
        [ -n "$name" ] || continue
        tab=$(field "$pane" 2 "$STATE/panes")
        ws=$(field "$pane" 3 "$STATE/panes")
        [ -n "$tab" ] || continue
        cwd=$(cat "$STATE/p${pane##*:p}.cwd" 2>/dev/null || true)
        [ $first -eq 1 ] || printf ','
        first=0
        printf '{"name":%s,"agent":%s,"agent_status":"idle","pane_id":%s,"tab_id":%s,"workspace_id":%s,"cwd":%s}' \
          "$(jstr "$name")" "$(jstr "$kind")" "$(jstr "$pane")" "$(jstr "$tab")" \
          "$(jstr "$ws")" "$(jstr "$cwd")"
      done <"$STATE/agents"
      printf ']}}\n'; }
    ;;

  "agent start")
    name=${POS[0]:?}
    pane=${FLAG[pane]:?}
    [ -n "$(field "$pane" 2 "$STATE/panes")" ] || fail no_such_pane "no pane $pane"
    printf '%s\t%s\t%s\n' "$name" "$pane" "${FLAG[kind]:-}" >>"$STATE/agents"
    echo '{"result":{}}'
    ;;

  "agent prompt"|"agent wait"|"agent send-keys"|"agent focus")
    # Nothing here has a turn to wait for or a keystroke to land: an agent is
    # a registration. Each answers as herdr does for a session that exists.
    [ -n "$(field "${POS[0]:-}" 1 "$STATE/agents")" ] || fail no_such_agent "no agent ${POS[0]:-}"
    echo '{"result":{}}'
    ;;

  "agent read")
    echo ""
    ;;

  "pane process-info")
    # The pane's shell is always in the foreground: no command step or agent
    # here ever holds it, so `wait_for_pane_shell` returns at once.
    echo '{"result":{"foreground":{"is_shell":true}}}'
    ;;

  "pane current")
    # `spoolway dispatch`'s own pane gate calls this before anything else —
    # see `commands::dispatch::check_dispatcher_visible` — and refuses the
    # whole run on any failure here, the same way it would over a bare
    # terminal with no real herdr pane at all. This double's caller is never
    # actually placed in a pane the way a person's shell would be — nothing
    # here can be, per this file's own header — so answering the gate is
    # what stands in for that placement, and every case in this suite that
    # needs a dispatcher to actually run relies on it: this suite's whole
    # premise is a dispatcher that *is* running where a person could see it,
    # same as every other backend it drives through this double.
    #
    # `HERDR_STUB_NO_PANE`, set by the one case that is about the gate
    # itself rather than about what a pane does, asks for the opposite: the
    # answer a bare terminal actually gets, so that refusal has something
    # real to run against instead of being proven only by a unit test that
    # already knows the answer it wants.
    #
    # A fixed, self-consistent triple rather than an empty `workspace_id`:
    # `Herdr::own_pane_id` (src/mux.rs) parses `pane_id` out of this same
    # reply now, for the fourth line `Lock::acquire` writes, and a shape
    # this double's caller could never actually have — no `pane_id` at
    # all — made that parse fail silently on every e2e dispatcher start.
    # Not registered in `$STATE/panes`: nothing here calls `pane get` on it,
    # and this double never places its caller in a pane it tracks for real —
    # see the header.
    if [ -n "${HERDR_STUB_NO_PANE:-}" ]; then
      fail no_current_pane "not running in a herdr pane"
    else
      echo '{"result":{"pane":{"pane_id":"self:p0","tab_id":"self:t0","workspace_id":"self"}}}'
    fi
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
    # and `mux::is_retired_shared_workspace` leans on that absence.
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

  "tab rename")
    tab=${POS[0]:?}
    label=${POS[1]:-}
    # Rewrite this tab's own line in place — the third field, its label —
    # leaving every other tab's line untouched. Real herdr answers the same
    # `{}` either way; this double has no notion of a tab id it does not
    # recognise to refuse against.
    awk -F'\t' -v OFS='\t' -v t="$tab" -v l="$label" \
      '$1==t {$3=l} {print}' "$STATE/tabs" >"$STATE/tabs.tmp"
    mv "$STATE/tabs.tmp" "$STATE/tabs"
    echo '{"result":{}}'
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
    pane=${POS[0]:?}
    label=${POS[1]:-}
    # Rewrite this pane's own line in place — the fourth field, its label —
    # the same way `tab rename` above rewrites a tab's third. Without this a
    # pane's label column keeps whatever `make_pane` wrote at creation, which
    # for a split pane is the empty string it was cut with.
    awk -F'\t' -v OFS='\t' -v p="$pane" -v l="$label" \
      '$1==p {$4=l} {print}' "$STATE/panes" >"$STATE/panes.tmp"
    mv "$STATE/panes.tmp" "$STATE/panes"
    echo '{"result":{}}'
    ;;

  "pane run")
    pane=${POS[0]:?}
    cmd=${POS[1]:-}
    n=${pane##*:p}
    fifo="$STATE/p$n.in"
    [ -p "$fifo" ] || fail no_such_pane "no pane $pane"
    # A real herdr pane garbles what is typed past a few hundred bytes on a
    # Mac (858 arrived whole, 1,140 did not), so the double refuses past the
    # same 512 `src/mux.rs` bounds every `pane run` by. Refused before
    # anything is kept or typed: a line over the bound never reached a shell.
    bytes=$(printf '%s' "$cmd" | wc -c | tr -d ' ')
    if [ "$bytes" -gt 512 ]; then
      fail line_too_long "pane $pane was typed $bytes bytes; the bound is 512"
    fi
    # The transcript. Every string spoolway typed into a pane, kept whole and
    # measured, so a case can assert what got typed rather than only what came
    # out the far end.
    slot=$(next_id)
    printf '%s' "$cmd" >"$STATE/typed/$slot"
    printf '%s\t%s\t%s\n' "$pane" "$bytes" \
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
