# shellcheck shell=bash
# The stand-in agents: what a lane runs when no model may be spent.
#
# Sourced by a suite, which then calls `install_mock_agents`. What goes on PATH
# is one binary per kind spoolway can launch — `pi` and `claude`, which the
# shipped agent profiles name, and `codex`, which a profile is pointed at by
# `agents.<profile>.kind` — so nothing about the dispatcher, the profiles or
# the guardrails is special-cased for a test. A lane really spawns, really
# reports, and really lands work. The codex stand-in is a thin front over
# `pi`: it reads the pieces its kind is handed differently (the prompt, the
# session home) and delegates the behaviour, so a suite can mix kinds without
# the modes drifting apart.
#
# Two behaviours, in priority order:
#
#   1. A pathology. `$CTL/<task>.<step>`, `$CTL/<task>`, or the task id itself
#      names one: crash, hang, vanish, bail, noland. These cannot be expressed
#      as a patch — a hang is the absence of an outcome — and they are what
#      force the dispatcher's counters and escalations.
#
#   2. A canned patch. `$SOLUTIONS/<task>/<step>.patch`, applied in the lane's
#      worktree, followed by the fixture's own test command. This is the mode
#      that makes a mock run mean something: the repo really builds, an overlap
#      really conflicts, and the archivist really has a diff to read. Record
#      these from a real-agent run rather than writing them by hand.
#
# A task with neither gets the generic marker-file work, which is enough for any
# scenario that is about the pipeline's shape rather than its content.
#
# ## The stand-ins are files
#
# `agents/pi` and `agents/claude` are ordinary scripts under version control,
# copied onto PATH by the function below and configured entirely through the
# environment. They used to be *generated* here, by a single unquoted heredoc
# three hundred lines long with three levels of backslash escaping — and its own
# comments warned twice about backticks and about `\$` counts, both from bugs
# that had actually happened. Adding one feature meant generating shell from
# shell from shell.
#
# Nothing about the ctl-file protocol changed. What changed is that `shellcheck`
# can see the stand-ins, a person can run one by hand, and a `$` means what it
# says.

HERE_AGENT_MOCK=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# install_mock_agents <bindir> <ctldir> [solutions] [fixture-test] [fixture-verify] [forge]
#
# Exports PATH, and the variables the stand-ins read. The caller keeps the ctl
# directory to change an agent's behaviour mid-run, which is how a task that
# crashed starts behaving once a person has "fixed" it.
install_mock_agents() {
  local bindir=$1 ctl=$2 solutions=${3:-} fixture_test=${4:-} fixture_verify=${5:-} forge=${6:-}
  mkdir -p "$bindir" "$ctl"

  install -m 755 "$HERE_AGENT_MOCK/agents/pi"     "$bindir/pi"
  install -m 755 "$HERE_AGENT_MOCK/agents/claude" "$bindir/claude"
  install -m 755 "$HERE_AGENT_MOCK/agents/codex"  "$bindir/codex"
  # Beside them, and named in the environment. A stand-in cannot find it off
  # `$0`: a lane is launched through PATH as a bare `pi`, so `$0` carries no
  # directory at all and `dirname` answers with the lane's own worktree.
  install -m 644 "$HERE_AGENT_MOCK/agents/transcript.sh" "$bindir/transcript.sh"
  export E2E_STANDIN_LIB="$bindir/transcript.sh"

  # What a stand-in is configured with. Exported rather than baked in, which is
  # the whole of what replaced the heredoc — and exported here rather than in
  # the suites, so a suite still says only what it needs.
  #
  # `E2E_FORGE_DIR` rather than `E2E_FORGE`: `new_forge` already exports `FORGE`
  # and `SPOOLWAY_E2E_FORGE`, and a third name for the same directory that
  # differed by one character from an existing one would be a trap.
  export E2E_CTL="$ctl"
  export E2E_SOLUTIONS="$solutions"
  export E2E_FIXTURE_TEST="$fixture_test"
  export E2E_FIXTURE_VERIFY="$fixture_verify"
  export E2E_FORGE_DIR="$forge"
  # The build under test, by absolute path. A lane's `spoolway report` must
  # reach the binary the suite is asserting about and never whatever is
  # installed on the machine.
  export E2E_SPOOLWAY="$SPOOLWAY"

  export PATH="$bindir:$PATH"
}

# The models the mock answers to.
#
# A step names its own model now, so this rewrites the shipped pipelines rather
# than setting `agents.*.model`, which is gone: config holds facts about this
# machine, and which model runs a step is a fact about the pipeline.
#
# The names are not arbitrary. The mock `claude` logs its argv and the pipelines
# suite reads that log back, checking that the model a step names is the model
# the lane was handed — so the two have to be told apart there, and a suite
# wanting a third model can `set_step_of` its way to one.
mock_models() {
  must "the mock's models" set_models fake-local fake-cloud
}
