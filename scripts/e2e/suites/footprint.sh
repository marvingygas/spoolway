#!/usr/bin/env bash
# The UI and the dispatcher stay inside a memory budget and a process budget,
# and their cost follows the number of lanes, never the size of the history.
#
# The budget is a base plus an allowance per lane, so a feature that really
# needs more memory raises a number below in its own pull request, where review
# sees it. A fixed ceiling would either block that feature or sit so far above
# the truth that a slow leak never reached it.
#
# Five lanes run against the herdr double, each registered and never reporting,
# so they stay where they are for as long as the suite measures. Each lane has a
# transcript at the path its session id names, as a running agent's would, so
# the dispatcher and the UI read it the way they do in real use. The suite
# measures twice: once with a small history, and once after it has written 5,000
# transcripts for each agent kind and a 40 MB ledger. The second run is the
# first one's twin, and the two must agree to within a tenth.
#
# What is measured:
#
#   peak memory      `VmHWM` in `/proc/<pid>/status`, the most the process has
#                    ever held. It is stable for a given run, where a sample of
#                    the current size is not, and a fresh dispatcher is started
#                    for each history so one run's peak is never the other's.
#   processes        shims for `git`, `herdr`, `gh` and `tmux` sit first on PATH.
#                    Each logs one line naming every ancestor of the call and
#                    then runs the real tool with the same arguments, so a
#                    spawn is attributed to the dispatcher or the UI by who was
#                    above it, and nothing a shim hides can pass.
#
# A dispatcher pass is one `herdr agent list` and whatever follows it up to the
# next one. Passes are counted that way because nothing else marks them from
# outside. A pass that makes more than one herdr call is the full sweep, which
# the dispatcher runs when a lane starts or ends and once a minute as a safety;
# one such pass in a window is that sweep, and a second is a regression.
#
# Limits: peak memory is read from Linux's `/proc`, so this does not run on
# macOS. Only the four tools above are counted as processes; a spawn of any
# other binary is not seen.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

# The budget. A change that needs more raises these here, in its own PR,
# and says why in that PR.
UI_BASE_MB=30          UI_PER_LANE_MB=2
DISPATCH_BASE_MB=20    DISPATCH_PER_LANE_MB=1
IDLE_GIT_SPAWNS=0      IDLE_HERDR_CALLS=1
FLAT_TOLERANCE_PCT=10  # small history vs. 5,000 transcripts and a 40 MB ledger
EVAL_SETTLE_S=5        # UI back under budget this long after leaving Eval

LANES=5
TRANSCRIPTS=5000
LEDGER_MB=40
# How long a window of idle passes is watched, and how long the process is left
# alone first. Three passes fit in a window at the dispatcher's ten-second rate,
# and the settle is longer than one pass so the sweep a fresh dispatcher makes
# on its first pass is over before the window opens.
SETTLE_S=15
WINDOW_S=35
UI_SETTLE_S=5
UI_WINDOW_S=5

LIVE=${WORK:-$(mktemp -d)}
export SPOOLWAY_SKIP_VERSION_CHECK=1

UI_LIMIT_MB=$((UI_BASE_MB + UI_PER_LANE_MB * LANES))
DISPATCH_LIMIT_MB=$((DISPATCH_BASE_MB + DISPATCH_PER_LANE_MB * LANES))

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$LIVE/ctl" "" "" "" "$FORGE"
new_repo "$LIVE/proj"
configure_project plan/footprint
publish plan/footprint
BODY="$LIVE/body.md"
task_body "$BODY"

# ------------------------------------------------ the double, and the shims
HSTATE="$LIVE/herdr-stub"
HERDRBIN="$LIVE/herdr-bin"
SHIMS="$LIVE/shims"
SPAWNS="$LIVE/spawns.log"
mkdir -p "$HSTATE" "$HERDRBIN" "$SHIMS"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
: > "$SPAWNS"

# One shim per counted tool. It writes a line of ancestor pids and arguments
# and then `exec`s the real tool, so the arguments, the exit code and every
# stream are the real tool's own: a shim that swallowed a failure would let a
# broken call pass as a quiet one. `herdr` resolves to the double, which is the
# only herdr this suite has.
make_shim() { # tool real-path
  local tool=$1 real=$2
  cat > "$SHIMS/$tool" <<SHIM
#!/usr/bin/env bash
anc=""
p=\$PPID
while [ -n "\$p" ] && [ "\$p" -gt 1 ] 2>/dev/null; do
  anc="\$anc \$p"
  p=\$(sed 's/^.*) //' "/proc/\$p/stat" 2>/dev/null | cut -d' ' -f2)
done
printf '%s|%s|%s|%s\n' "$tool" "\${anc# }" "\$*" "\$EPOCHREALTIME" >> "$SPAWNS"
exec "$real" "\$@"
SHIM
  chmod +x "$SHIMS/$tool"
}
for tool in git gh tmux; do
  real=$(command -v "$tool" 2>/dev/null) && make_shim "$tool" "$real"
done
make_shim herdr "$HERDRBIN/herdr"
PATH="$SHIMS:$PATH"; export PATH

must "the herdr backend" "$SPOOLWAY" config set dispatch.backend herdr
must "the lane patience" "$SPOOLWAY" config set dispatch.lane_quiet 1h
must "room for every lane" "$SPOOLWAY" config set dispatch.visible_lanes "$LANES"

# ------------------------------------------------------------ five held lanes
# Each lane runs a step of its own, in a pipeline of its own. An agent is named
# `<step>-<tag>`, the tag being the leading characters of a hash of the task id
# (`src/lane_alias.rs`). Ids that differ only in their last character, as
# `lane1` to `lane5` do, share every tag prefix the name may use, so a step
# offers them three names between them and five lanes in one step could not
# all start. The lanes are queued one at a time, each waited for before the
# next, so the suite knows every one is recorded before it measures.
MODEL=$(awk '/id: implement/ {found=1} found && /model:/ {print $2; exit}' \
  .spoolway/pipelines/default.yml)
n=0
while [ "$n" -lt "$LANES" ]; do
  n=$((n + 1))
  cat > ".spoolway/pipelines/held$n.yml" <<YML
description: One agent step that never reports, so its lane stays up.

steps:
  - id: hold$n
    description: Held open for the whole suite.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: done
YML
done
must "the pipelines check out" "$SPOOLWAY" pipeline check

lanes_recorded() { [ "$(grep -c '"started_at":' "$SPOOLWAY_PROJECT_HOME/lanes.json" 2>/dev/null)" -ge "$1" ]; }
dispatcher_start
n=0
while [ "$n" -lt "$LANES" ]; do
  n=$((n + 1))
  task_doc "$LIVE/lane$n.md" "lane$n" "$BODY" "group: lane$n" "pipeline: held$n"
  must "lane$n queued" "$SPOOLWAY" queue add --from "$LIVE/lane$n.md"
  must "lane$n running" poll_until 90 lanes_recorded "$n"
done

# A transcript for each lane at the path its session id names, one assistant
# turn in the shape `pi` writes. Every held pipeline runs `pi`, so that is the
# only kind written.
write_lane_transcripts() {
  local session
  mkdir -p "$HOME/.pi/agent/sessions/e2e"
  awk -F'"' '/"session":/ {print $4}' "$SPOOLWAY_PROJECT_HOME/lanes.json" \
    | while read -r session; do
      printf '{"type":"message","id":"t1","message":{"role":"assistant","model":"fake-local","usage":{"input":1200,"output":16,"cacheRead":0,"cacheWrite":0}}}\n' \
        > "$HOME/.pi/agent/sessions/e2e/2020-01-01T00-00-00_$session.jsonl"
    done
}
write_lane_transcripts

# The history this suite writes is deleted when it ends, a run that failed
# included, so a suite run on its own does not leave 40 MB and ten thousand
# files in a scratch directory. `lib.sh`'s own steps stay in the trap, so the
# dispatcher and screen still stop and the tally still reaches `run.sh`.
drop_history() {
  rm -rf "$HOME"/.claude/projects/h[0-9]* "$HOME"/.pi/agent/sessions/h[0-9]*
  : > "$SPOOLWAY_PROJECT_HOME/usage.jsonl" 2>/dev/null
  true
}
trap 'screen_stop; dispatcher_stop; lanes_stop; drop_history; record_results' EXIT

# ------------------------------------------------------------ measurements
peak_kb() { awk '/^VmHWM:/ {print $2}' "/proc/$1/status" 2>/dev/null; }
now_kb()  { awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null; }
mb() { echo $((($1 + 1023) / 1024)); } # kB to whole MB, rounded up

dispatch_pid() { pgrep -P "$E2E_DISPATCHER_PID" -f ' dispatch$' | head -n 1; }

# Lines of the spawn log from line $1 on, made by a process under pid $2.
spawns_under() { # from-line pid
  tail -n "+$1" "$SPAWNS" | awk -F'|' -v pid="$2" '
    { n = split($2, a, " "); for (i = 1; i <= n; i++) if (a[i] == pid) { print; break } }'
}

# Reads the spawns of one window into per-pass counts: how many git, and how
# many herdr, calls fall between one `herdr agent list` and the next.
#   passes   how many passes the window held
#   git      git spawns anywhere in the window
#   sweeps   passes that made more than IDLE_HERDR_CALLS herdr calls
#   median   the middle pass's count of all spawns, for the flatness check
analyse_passes() { # spawn-lines-file
  awk -F'|' -v max="$IDLE_HERDR_CALLS" '
    function close_pass() {
      if (!started) return
      passes++
      sizes[passes] = total
      if (herdr > max) sweeps++
    }
    {
      # Counted before the first pass starts: a git spawn between the window
      # opening and its first `agent list` is still a spawn in the window.
      if ($1 == "git") git++
      if ($1 == "herdr" && $3 == "agent list") { close_pass(); started = 1; total = 0; herdr = 0 }
      if (!started) next
      total++
      if ($1 == "herdr") herdr++
    }
    END {
      close_pass()
      # Sorted by hand: `asort` is a gawk extension, and the awk on Debian and
      # Ubuntu runners is mawk.
      for (i = 2; i <= passes; i++) {
        v = sizes[i]
        for (j = i - 1; j >= 1 && sizes[j] > v; j--) sizes[j + 1] = sizes[j]
        sizes[j + 1] = v
      }
      printf "passes=%d git=%d sweeps=%d median=%d\n", passes + 0, git + 0, sweeps + 0, (passes ? sizes[int((passes + 1) / 2)] : 0)
    }' "$1"
}

# Results of one history, filled in by `measure`: indexed by history name.
declare -A D_PEAK UI_PEAK PER_PASS
HINTS=()
hint() { # key text
  case " ${HINT_KEYS:-} " in *" $1 "*) return 0 ;; esac
  HINT_KEYS="${HINT_KEYS:-} $1"
  HINTS+=("$2")
}

measure() { # history-name
  local h=$1 dpid kb mark facts passes git sweeps median
  dispatcher_restart
  sleep "$SETTLE_S"
  dpid=$(dispatch_pid)
  mark=$(($(wc -l < "$SPAWNS") + 1))
  sleep "$WINDOW_S"
  if [ -z "$dpid" ] || [ "$dpid" != "$(dispatch_pid)" ]; then
    printf '  \033[31mSETUP\033[0m the dispatcher restarted while it was measured (%s)\n' "$h" >&2
    exit 2
  fi
  spawns_under "$mark" "$dpid" > "$LIVE/window.$h"
  D_PEAK[$h]=$(peak_kb "$dpid")
  facts=$(analyse_passes "$LIVE/window.$h")
  eval "$(echo "$facts" | sed 's/ /; /g')"
  PASSES[$h]=$passes GIT[$h]=$git SWEEPS[$h]=$sweeps PER_PASS[$h]=$median
  dispatcher_stop

  # The tab the screen opens on, and where it sits when nobody is using it. The
  # dispatch tab is not measured: it asks herdr for its agents every second.
  screen_start "$LIVE/screen-$h.log"
  sleep "$UI_SETTLE_S"
  mark=$(($(wc -l < "$SPAWNS") + 1))
  sleep "$UI_WINDOW_S"
  UI_SPAWNS[$h]=$(spawns_under "$mark" "$E2E_SCREEN_PID" | wc -l)
  # Read last, so whatever the UI took while it sat idle is in its peak.
  UI_PEAK[$h]=$(peak_kb "$E2E_SCREEN_PID")
}
declare -A PASSES GIT SWEEPS UI_SPAWNS

# The history grows by adding to what is already there, in the suite's scratch
# home. `drop_history`, in the exit trap, takes it away again. Many
# small files rather than a few large ones: what grows is the number of
# transcripts a walk has to visit and the number of lines a ledger read has to
# parse, and a few big files would grow neither.
grow_history() {
  local dir i kind
  for kind in claude pi; do
    i=0
    while [ "$i" -lt "$TRANSCRIPTS" ]; do
      dir="$HOME/.$kind/projects/h$((i % 50))"
      [ "$kind" = pi ] && dir="$HOME/.pi/agent/sessions/h$((i % 50))"
      mkdir -p "$dir"
      id=$(printf '%08x-0000-4000-8000-000000000000' "$i")
      if [ "$kind" = pi ]; then
        file="$dir/2020-01-01T00-00-00_$id.jsonl"
        line='{"type":"message","id":"t1","message":{"role":"assistant","model":"fake-local","usage":{"input":900,"output":16,"cacheRead":0,"cacheWrite":0}}}'
      else
        file="$dir/$id.jsonl"
        line='{"type":"assistant","requestId":"r1","timestamp":"2026-01-01T00:00:00Z","message":{"model":"fake-cloud","usage":{"input_tokens":900,"output_tokens":16}}}'
      fi
      printf '%s\n%s\n%s\n%s\n' "$line" "$line" "$line" "$line" > "$file"
      i=$((i + 1))
    done
  done
  # A ledger a few tasks wide and very long: what must not matter to the
  # process is how many lines it has, and one entry per task is what totals are
  # kept in.
  awk -v mb="$LEDGER_MB" -v month="$(date -u +%Y-%m)" 'BEGIN {
    bytes = 0; i = 0
    while (bytes < mb * 1048576) {
      i++
      line = sprintf("{\"ts\": \"%s-%02dT%02d:%02d:00+00:00\", \"task\": \"lane%d\", \"step\": \"implement\", \"pipeline\": \"default\", \"agent\": \"pi\", \"kind\": \"pi\", \"model\": \"fake-local\", \"session\": \"%08x-0000-4000-8000-000000000001\", \"round\": 0, \"wall_s\": 60, \"turns\": 3, \"tokens\": {\"input\": 900, \"output\": 40, \"cache_read\": 4000, \"cache_write_5m\": 0, \"cache_write_1h\": 0, \"reasoning\": 0}, \"cost_usd\": 0.0125, \"version\": \"e2e\", \"commit\": \"e2e\", \"skill\": \"\"}\n", month, 1 + i % 28, i % 24, i % 60, 1 + i % 5, i % 1000)
      printf "%s", line
      bytes += length(line)
    }
  }' >> "$SPOOLWAY_PROJECT_HOME/usage.jsonl"
}

# ------------------------------------------------------------ the small history
measure small

# ------------------------------------------------------------ the large history
screen_stop
grow_history
measure large

# ------------------------------------------------------------ the Eval tab
# Eval reads the ledger and the sessions on a background thread, so it may go
# over budget while it does. What it may not do is stay there: freed memory has
# to go back to the system within a few seconds of leaving. The screen of the
# large history is still open, on the queue tab; Eval is three tabs to its
# right.
eval_back_mb=""
screen_press $'\x1b[C'   # routines
screen_press $'\x1b[C'   # jobs
screen_press $'\x1b[C'   # eval
# Left only once the load is over. Leaving mid-load would trim while the load
# thread still holds its memory, and the check would then fail at random on a
# slow runner. The popup is waited for first, briefly, so a frame drawn before
# Eval began is not mistaken for a finished load; a load too quick to be seen
# has finished by the time it is asked about.
eval_loading() { screen_last_frame | grep -qa 'Loading…'; }
poll_until 3 eval_loading
if ! poll_while 120 eval_loading; then
  printf '  \033[31mSETUP\033[0m the Eval tab was still loading after 120 s\n' >&2
  exit 2
fi
screen_press $'\x1b[D'   # back to jobs
settled=0
for ((i = 0; i <= EVAL_SETTLE_S * 10; i++)); do
  kb=$(now_kb "$E2E_SCREEN_PID")
  eval_back_mb=$(mb "${kb:-0}")
  if [ "${kb:-0}" -le $((UI_LIMIT_MB * 1024)) ]; then settled=1; break; fi
  sleep 0.1
done

# ------------------------------------------------------------ the verdicts
check_under() { # label peak-kb limit-mb formula
  local label=$1 kb=$2 limit=$3 formula=$4
  if [ "${kb:-0}" -le $((limit * 1024)) ] && [ -n "$kb" ]; then
    ok "$label peak RSS under $limit MB ($formula)"
  else
    bad "$label peak RSS under $limit MB ($formula): peak was $(mb "${kb:-0}") MB"
    return 1
  fi
}

check_under dispatcher "${D_PEAK[small]}" "$DISPATCH_LIMIT_MB" \
  "$DISPATCH_BASE_MB + $DISPATCH_PER_LANE_MB × $LANES lanes" \
  || hint dispatch-over "Dispatcher over budget. Find what grew, or, if the growth is wanted, raise
DISPATCH_BASE_MB or DISPATCH_PER_LANE_MB in scripts/e2e/suites/footprint.sh
in the same PR and say why there."
check_under UI "${UI_PEAK[small]}" "$UI_LIMIT_MB" \
  "$UI_BASE_MB + $UI_PER_LANE_MB × $LANES lanes" \
  || hint ui-over "UI over budget. Find what grew, or, if the growth is wanted, raise
UI_BASE_MB or UI_PER_LANE_MB in scripts/e2e/suites/footprint.sh
in the same PR and say why there."

idle_what="idle dispatcher pass starts $IDLE_GIT_SPAWNS git and at most $IDLE_HERDR_CALLS herdr call"
if [ "${PASSES[small]}" -ge 2 ] && [ "${GIT[small]}" -le "$IDLE_GIT_SPAWNS" ] \
  && [ "${SWEEPS[small]}" -le 1 ]; then
  ok "$idle_what"
else
  bad "$idle_what: ${PASSES[small]} passes started ${GIT[small]} git and ${SWEEPS[small]} passes of more than $IDLE_HERDR_CALLS herdr call"
  hint idle-spawn "An idle dispatcher pass started a process it should not have. Read
$LIVE/window.small for what, and who started it. A pass that finds nothing
changed should stat its files and ask herdr once. IDLE_GIT_SPAWNS and
IDLE_HERDR_CALLS are not for raising lightly: they are the point of the check."
fi

if [ "${UI_SPAWNS[small]}" -eq 0 ]; then
  ok "idle UI second starts no process"
else
  bad "idle UI second starts no process: ${UI_SPAWNS[small]} started in $UI_WINDOW_S s"
  hint ui-spawn "The idle UI started a process. Something polled with git, herdr, gh or tmux
on a tick that should only have looked at file sizes and times."
fi

# Whether two peaks, in kB, are within FLAT_TOLERANCE_PCT of each other.
flat() { # small large
  [ -n "$1" ] && [ -n "$2" ] || return 1
  local diff=$(($2 > $1 ? $2 - $1 : $1 - $2))
  [ $((diff * 100)) -le $(($1 * FLAT_TOLERANCE_PCT)) ]
}

flat_what() { # label what
  echo "$1 peak RSS flat (within $FLAT_TOLERANCE_PCT%) with $2"
}
if flat "${D_PEAK[small]}" "${D_PEAK[large]}"; then
  ok "$(flat_what dispatcher "5,000 transcripts")"
else
  bad "$(flat_what dispatcher "5,000 transcripts"): $(mb "${D_PEAK[small]}") MB → $(mb "${D_PEAK[large]}") MB"
  hint dispatch-flat "Dispatcher cost grew with history, not with lanes. Look for a walk
of ~/.claude/projects or a whole-file read per pass. This check has
no number to raise."
fi
if flat "${UI_PEAK[small]}" "${UI_PEAK[large]}"; then
  ok "$(flat_what UI "a $LEDGER_MB MB ledger")"
else
  bad "$(flat_what UI "a $LEDGER_MB MB ledger"): $(mb "${UI_PEAK[small]}") MB → $(mb "${UI_PEAK[large]}") MB"
  hint ui-flat "UI cost grew with history, not with lanes. Look for a ledger line or a
transcript kept in memory when a running total would do. This check has
no number to raise."
fi

procs_what="processes per idle pass unchanged with 5,000 transcripts"
# The UI's idle second is judged here for the large history too: a tick that
# only starts a process once the history is big is cost following history.
if [ "${PER_PASS[small]}" -eq "${PER_PASS[large]}" ] \
  && [ "${GIT[large]}" -le "$IDLE_GIT_SPAWNS" ] \
  && [ "${UI_SPAWNS[large]}" -eq 0 ]; then
  ok "$procs_what"
else
  bad "$procs_what: ${PER_PASS[small]} → ${PER_PASS[large]} per pass, ${GIT[large]} git in the large run, ${UI_SPAWNS[large]} started by the idle UI in $UI_WINDOW_S s"
  hint procs-flat "Dispatcher passes or the idle UI started more processes with a large history.
Read $LIVE/window.large for the dispatcher. This check has no number to raise."
fi

eval_what="UI back under $UI_LIMIT_MB MB within $EVAL_SETTLE_S s of leaving Eval"
if [ "$settled" -eq 1 ]; then
  ok "$eval_what"
else
  bad "$eval_what: still $eval_back_mb MB after $EVAL_SETTLE_S s"
  hint eval-settle "Memory from the Eval tab was not given back. Large reads should happen
on a background thread whose memory is returned to the system when the load
ends. This check has no number to raise except EVAL_SETTLE_S, and a longer
wait only hides the leak."
fi

if [ "${#HINTS[@]}" -gt 0 ]; then
  echo
  for h in "${HINTS[@]}"; do
    printf '%s\n' "$h" | sed 's/^/  /'
  done
fi

finish
