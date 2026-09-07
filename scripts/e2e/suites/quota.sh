#!/usr/bin/env bash
# A quota probe already at or above its own ceiling.
#
# `implement` runs `pi`, which carries no quota probe, so a task reaches
# `review` — the step `claude` runs — normally. With `agents.claude.
# quota_ceiling` set and a fixture `~/.claude.json` above it, no `review`
# lane starts: the task is parked instead, `parked_until:` written straight
# from the probe's own `resets_at`. The clock that reading names is the same
# one a real reset would move, so waiting for it to pass is the same test as
# a dispatcher restarted days later against a moved reading would be, at this
# harness's own compressed clock — and once it has, the very next pass picks
# the task straight back up.
#
# covers: agents.<profile>.quota_ceiling — a fixture reading above the ceiling parks a new lane rather than starting one, and the task is picked up once its own clock passes
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/quota "$LIVE/worktrees"
publish plan/quota

must "the quota ceiling" "$SPOOLWAY" config set agents.claude.quota_ceiling 85

# Shaped like Claude Code's own cache — see `crate::quota::read`'s own doc.
# The five-hour window resets a handful of seconds out: close enough that
# waiting for it is a suite's whole budget, not a wall clock nobody would sit
# through.
RESETS_AT=$(date -u -d "@$(($(date +%s) + 5))" +%Y-%m-%dT%H:%M:%SZ)
cat > "$HOME/.claude.json" <<JSON
{"cachedUsageUtilization": {
  "fetchedAtMs": $(($(date +%s) * 1000)),
  "utilization": {
  "five_hour": {"utilization": 88, "resets_at": "$RESETS_AT"},
  "seven_day": {"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}
}}}
JSON

BODY="$LIVE/body.md"
task_body "$BODY"
task_doc "$LIVE/quota.md" quota "$BODY" "group: quota"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/quota.md"

if drive quota review 40; then ok "the task reaches \`review\`, staffed by \`claude\`"
else bad "the task reaches \`review\`, staffed by \`claude\` (stuck at \`$(stage_of quota)\`)"; fi

if wait_for_text 10 "$SPOOLWAY_PROJECT_HOME/queue/quota.md" "parked_until:"
then ok "the ceiling parks the task instead of starting a lane"
else bad "the ceiling parks the task instead of starting a lane"; front quota | sed 's/^/        /'; fi

if [ "$(stage_of quota)" = review ]; then ok "it stays on \`review\` while parked"
else bad "it stays on \`review\` while parked (moved to \`$(stage_of quota)\`)"; fi

if lane_on_record "quota · review"; then bad "no \`claude\` lane was started while it was parked"
else ok "no \`claude\` lane was started while it was parked"; fi

has "the park names the probe's own reading" "88%" "$SPOOLWAY_PROJECT_HOME/queue/quota.md"

# Move the fixture reading itself, below the ceiling, rather than leaving
# the original 88% sitting there for `parked_until`'s own clock to outlive
# by coincidence. A pass never rereads the probe once a task is parked —
# `parked_until` is the only clock it consults — so this reading being fresh
# and clear proves nothing on its own; it rules out the read having been the
# thing that let the task through, so the pickup below can only be
# `parked_until` running out.
cat > "$HOME/.claude.json" <<JSON
{"cachedUsageUtilization": {
  "fetchedAtMs": $(($(date +%s) * 1000)),
  "utilization": {
  "five_hour": {"utilization": 40, "resets_at": "2099-01-01T00:00:00Z"},
  "seven_day": {"utilization": 10, "resets_at": "2099-01-01T00:00:00Z"}
}}}
JSON

if drive quota gone 40; then ok "once the clock passes, the task is picked up and runs to \`done\`"
else bad "once the clock passes, the task is picked up and runs to \`done\` (stuck at \`$(stage_of quota)\`)"; fi

finish
