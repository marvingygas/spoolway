Mirrors Spoolway task `${SPOOLWAY_TASK}` in group `${SPOOLWAY_GROUP}`.

- Source: `${SPOOLWAY_SOURCE}`
- Branch: `${SPOOLWAY_BRANCH}`
- Blocked by: `${SPOOLWAY_DEPENDS_TICKETS}`

The shell hook appends the task document below when this issue opens. Later
blocked or paused events add the current task document as a comment. Spoolway
does not emit an event for every pipeline step. Reaching `done` means the pull
request is ready for review; GitHub closes this issue only after that PR merges.
