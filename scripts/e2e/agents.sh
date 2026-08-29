# shellcheck shell=bash
# What runs in a lane while a suite is watching: the stand-ins, always.
#
# Sourced by every suite in place of the file behind it. There used to be a
# second mode here — `--agents real`, a suite's lanes against this machine's own
# model server — and with it a `mock_only` vocabulary for the scenarios a model
# cannot be asked to perform. Both are gone. Whether a model can do the work is
# a question the plans under scripts/e2e/plans/ answer, in a project
# scripts/e2e/scaffold.sh builds, with somebody watching; what is left here
# answers whether the dispatcher decides correctly, which is decidable from
# files and exit codes and costs nothing.
#
# So this file is a seam rather than a switch: one call site for installing the
# stand-ins and one for pointing the pipelines at the models they answer to,
# because a suite should say what it needs and never how it is arranged.

HERE_AGENTS=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=agent-mock.sh
source "$HERE_AGENTS/agent-mock.sh"

# install_agents <bindir> <ctldir> [solutions] [fixture-test] [fixture-verify] [forge]
install_agents() { install_mock_agents "$@"; }

# agent_models — the models the stand-ins answer to.
agent_models() { mock_models; }

# agent_sandbox — retained as a no-op call site.
#
# There is no kernel sandbox left to turn off: a lane is a prompt, task and
# agent with no boundary, confined by nothing this project configures. Kept
# as a function so callers do not need to know that changed.
agent_sandbox() {
  return 0
}

# handed_over <task> [base]
#
# Whether a task's change was handed over: its branch is on the remote, and a
# pull request exists for it against something.
#
# This is what a landing became. A task no longer merges into its base — it
# opens a pull request stacked on its dependency's — so there is no merge commit
# to look for, and the observable is the branch and the pull request.
handed_over() {
  local task=$1 branch="task/$1"
  git ls-remote --heads origin 2>/dev/null | grep -q "refs/heads/$branch" || return 1
  [ -n "${FORGE:-}" ] || return 0
  grep -lx "head=$branch" "$FORGE"/prs/[0-9]* 2>/dev/null | grep -q .
}

# stacked_on <task> <branch>
#
# Whether the task's pull request targets `branch`. The ancestry is the point:
# a stacked pull request whose base is the task's own `base:` rather than its
# dependency's branch is a flat one.
stacked_on() {
  local branch="task/$1" onto=$2 pr
  [ -n "${FORGE:-}" ] || return 1
  for pr in "$FORGE"/prs/[0-9]*; do
    [ -f "$pr" ] || continue
    grep -qx "head=$branch" "$pr" && grep -qx "base=$onto" "$pr" && return 0
  done
  return 1
}
