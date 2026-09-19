# shellcheck shell=sh
# One assistant turn, written where `usage::session_file` looks for this kind's
# transcripts. Sourced by both stand-ins.
#
# A stand-in has no transcript of its own, and ordinarily that is the right
# answer: a mock turn spends nothing, and every suite but one is better off with
# `carried_session` finding nothing to size. The exception is the suite about
# the context-window settings, which cannot reach `session_reuse_ctx`,
# `models.<model>.context_window` or `session_reuse_idle` at all unless
# something has written a transcript for the lookup to find, size and date.
#
# So it is off by default and switched on by a file. `$E2E_CTL/transcript` holds
# the input tokens of the turn to write, and an optional second line backdates
# the file's own mtime by that many seconds — which is the only way to make a
# session read stale, and so the only way to reach `session_reuse_idle`
# headlessly. `touched_at` reads the store's mtime, not any record inside it,
# so backdating means moving the file's clock and nothing about its content.

# write_transcript <kind> <argv...>
write_transcript() {
  kind=$1
  shift
  [ -n "${E2E_CTL:-}" ] && [ -s "$E2E_CTL/transcript" ] || return 0

  # The session id out of argv rather than the environment: it is the
  # `--session-id` the adapter renders, or the `--resume` it is swapped for on a
  # lane that is continuing one, and a stand-in that read only the first would
  # write a second file for every carried session and prove the opposite of what
  # the suite is asking.
  sid=""
  next=""
  for arg in "$@"; do
    [ "$next" = yes ] && { sid=$arg; next=""; }
    case "$arg" in --session-id | --resume) next=yes ;; esac
  done
  [ -n "$sid" ] || return 0

  tokens=$(sed -n 1p "$E2E_CTL/transcript")
  age=$(sed -n 2p "$E2E_CTL/transcript")
  age=${age:-0}

  # The directory under the sessions root is arbitrary on purpose. Both real
  # agents shard by an escaping of the lane's working directory; the lookup
  # walks every directory rather than reproducing that escaping, and this is the
  # same fact from the other side.
  case "$kind" in
    pi)
      dir="$HOME/.pi/agent/sessions/e2e"
      file="$dir/2020-01-01T00-00-00_$sid.jsonl"
      ;;
    claude)
      dir="$HOME/.claude/projects/e2e"
      file="$dir/$sid.jsonl"
      ;;
    *) return 0 ;;
  esac
  mkdir -p "$dir"

  # Each turn its own id: the harvest dedups a request repeated across content
  # blocks, and two turns sharing one would be banked as one.
  turn=$(( $(wc -l < "$file" 2>/dev/null || echo 0) + 1 ))

  case "$kind" in
    pi)
      printf '{"type":"message","id":"t%s","message":{"role":"assistant","model":"fake-local","usage":{"input":%s,"output":16,"cacheRead":0,"cacheWrite":0}}}\n' \
        "$turn" "$tokens" >> "$file"
      ;;
    claude)
      stamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)
      printf '{"type":"assistant","requestId":"r%s","timestamp":"%s","message":{"model":"fake-cloud","usage":{"input_tokens":%s,"output_tokens":16,"cache_creation":{"ephemeral_5m_input_tokens":1}}}}\n' \
        "$turn" "$stamp" "$tokens" >> "$file"
      ;;
  esac

  # `touched_at` is the store's mtime, so a stale store is a file whose clock
  # has been moved back — not anything written inside it.
  if [ "$age" -gt 0 ] 2>/dev/null; then
    then=$(date -u -d "-$age seconds" +%Y%m%d%H%M.%S 2>/dev/null)
    [ -n "$then" ] && touch -t "$then" "$file"
  fi
}

# ------------------------------------------------------- a pane that outlives
# `Mux::start_lane` respawns a tmux pane *as the agent itself*, so the pane
# closes the instant the agent exits — and spoolway is still mid-launch when
# it does: `Tmux::prompt` pastes the opening prompt, waits 300ms, and sends
# Enter. A real agent is sitting at its own prompt by then and will be for as
# long as a person leaves it there. A stand-in that has already reported and
# returned is gone, `send-keys` fails with `can't find pane`, and the launch
# is recorded as a failure the task cannot recover from — `MAX_LAUNCHES` is
# one, so it lands on `blocked` with a reason that has nothing to do with what
# the scenario was asking.
#
# So a suite that runs a real multiplexer can ask its stand-ins to sit still
# for a moment before they go, the way the thing they stand in for would.
# Nothing else sets this, and a headless or herdr lane never reads it: the
# tier's own speed is what this whole change is about, and a lane that
# lingered everywhere would spend it back.
if [ -n "${E2E_PANE_LINGER:-}" ]; then
  trap 'sleep "$E2E_PANE_LINGER"' EXIT
fi
