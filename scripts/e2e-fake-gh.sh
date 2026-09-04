#!/usr/bin/env bash
# Enough of `gh` for spoolway's `pr` and `merge` steps to run against a bare repo
# on disk instead of GitHub.
#
# The end-to-end test is about the pipeline's plumbing — that a lane reaches the
# forge only where the pipeline says so, that a lane reports an outcome, that the
# dispatcher moves the task on — and none of that is GitHub's. Talking to a real
# forge only bought the create-and-delete dance around every run, so
# `scripts/e2e/fixture.sh` and `scripts/e2e/scaffold.sh` install this file at
# `$FORGE/bin/gh` and point the run's `PATH` at it.
#
# It is a test double, not a `gh` implementation. Its failure policy is the
# thing to understand: **a run must reach `done`**. Watching a task go all the
# way through is the whole point of the sandbox, so a gap in this double must
# never be what stops one. Anything it does not implement, or is called with in
# a shape it does not expect, is recorded in `$FORGE/warnings.log` and waved
# through with a zero exit — never refused. A refusal here would fail the lane,
# route the task to `fix`, spend its `loop:` rounds and park it in `blocked`,
# which looks exactly like a pipeline bug and is not one.
#
# Two things are still hard failures, because waving them through would make
# the run a fiction rather than a test: a merge that genuinely conflicts, and a
# branch that exists nowhere at all. Both route to `blocked`, which is the
# pipeline behaving correctly — a person is genuinely needed.
#
# Reached through `PATH`, ahead of any real `gh`. The run sets no forge token,
# so a lane that tried a real GitHub call would fail on the missing credential
# rather than reach the network — this double is what every `gh` in the run
# resolves to instead.
#
#   $SPOOLWAY_E2E_FORGE/origin.git   the bare repo standing in for the remote
#   $SPOOLWAY_E2E_FORGE/prs/<n>      one pull request, as key=value lines
set -euo pipefail

# Installed at `$FORGE/bin/gh`, so the forge is the parent of this file's
# directory. Locating itself rather than reading the environment is what keeps
# this working inside a lane: a lane's pane gets no say in its own env, and
# `spoolway config set` has no syntax for a per-agent variable anyway.
FORGE=${SPOOLWAY_E2E_FORGE:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
ORIGIN="$FORGE/origin.git"
PRS="$FORGE/prs"
mkdir -p "$PRS"

# A genuine dead end: the run cannot continue and mean anything. Rare on
# purpose — see the failure policy above.
die() { note "FAILED: $*"; echo "fake-gh: $*" >&2; exit 1; }

# Something unexpected, waved through. The lane sees success and the pipeline
# keeps moving; the run's oddities are read afterwards, out of the log.
shrug() { note "waved through: $*"; echo "fake-gh: $* (waved through)" >&2; }

note() { printf '%s\t%s\n' "$(date -Is)" "$*" >> "$FORGE/warnings.log"; }

# Read one field out of a stored pull request.
field() { sed -n "s/^$2=//p" "$PRS/$1"; }

# One pull request as a JSON object, holding the fields asked for by name.
#
# `gh` takes a comma-separated field list and answers with exactly those, so a
# prompt that asks for `number,url,state` must not be handed `title` as well
# and left to guess. Everything here is a string or a number; nothing `gh`
# returns for these fields is nested, so no encoder is needed beyond quoting.
json_for() {
  local n=$1 want=$2 first=1 key value
  printf '{'
  for key in $(printf '%s' "$want" | tr ',' ' '); do
    case "$key" in
      number)      value=$n ;;
      url)         value="file://$ORIGIN/pull/$n" ;;
      state)       value=$(field "$n" state) ;;
      title)       value=$(field "$n" title) ;;
      # One line of it: the body is markdown a prompt wrote, and newlines
      # inside a JSON string need encoding this has no business doing.
      body)        value=$(head -1 "$PRS/$n.body" 2>/dev/null | tr -d '"\\') ;;
      baseRefName) value=$(field "$n" base) ;;
      headRefName) value=$(field "$n" head) ;;
      headRefOid)  value=$(field "$n" head_sha) ;;
      isDraft)     value=false ;;
      mergeable)   value=MERGEABLE ;;
      *) shrug "pr view/list asked for unknown json field: $key"; continue ;;
    esac
    [ "$first" = 1 ] || printf ','
    first=0
    case "$key" in
      number|isDraft) printf '"%s":%s' "$key" "$value" ;;
      *)              printf '"%s":"%s"' "$key" "$value" ;;
    esac
  done
  printf '}'
}

# The highest pull request number so far, or nothing. Bodies are stored beside
# the records as `<n>.body`, so only the bare numbers count.
latest() { ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n | tail -1; }

# The pull request a verb was pointed at, defaulting to the most recent.
#
# A number that names nothing falls back to the newest pull request rather than
# refusing: a prompt quoting the wrong number is not worth ending a run over,
# and in this sandbox there is only ever a handful. With none at all there is
# nothing to fall back to, which is one of the two genuine dead ends.
# The open pull request for a branch, if it has one.
pr_for_branch() {
  local branch=$1 n
  for n in $(ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n); do
    [ "$(field "$n" head)" = "$branch" ] || continue
    [ "$(field "$n" state)" = OPEN ] || continue
    echo "$n"; return 0
  done
  return 1
}

pr_number() {
  local n=${1:-} newest branch
  newest=$(latest)
  [ -n "$newest" ] || die "there are no pull requests in this sandbox forge"
  if [ -z "$n" ]; then
    # No number means *this branch's* pull request, which is what `gh` resolves
    # it to and the only reading that answers the question `spoolway stack` is
    # actually asking: "do I already have one?"
    #
    # Falling back to the newest instead is not a harmless default — it hands a
    # lane somebody else's pull request and tells it that one is its own. A real
    # run did exactly that: `spoolway stack` got `base`'s pull request back
    # while standing on `task/top`, concluded it already had one, and never
    # opened its own. The stack was a rung short and every report read as a pass.
    branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)
    if [ -n "$branch" ] && n=$(pr_for_branch "$branch"); then
      echo "$n"; return
    fi
    echo "$newest"
    shrug "asked about no pull request in particular from '${branch:-an unknown branch}', which has none; using #$newest"
    return
  fi
  if [ ! -f "$PRS/$n" ]; then
    shrug "no pull request $n; using #$newest instead"
    n=$newest
  fi
  echo "$n"
}

case "${1:-}" in
  # `gh auth status` is a liveness check; this forge needs no credentials.
  auth) exit 0 ;;
  pr) ;;
  # `gh stack`, the extension `spoolway stack` lands a plan with.
  #
  # Two findings from live repos are what this stands in for, and they are the
  # ones that must reach the handover step: `gh pr merge` is *refused* on a stacked pull
  # request (it wants the async merge API) and so is auto-merge. `gh stack merge
  # --yes` is the only thing that lands one, and merging the bottom retargets
  # the rest — which GitHub does unprompted.
  stack)
    # `$2`, not `$3`: the subcommand is the word after `stack`, and anything
    # after that is its arguments — `link <stack#> <branch>`, `merge --yes`.
    case "${2:-}" in
      link|"")
        # Linking is idempotent, and a clone with no stack state of its own can
        # link pull requests it never opened. Recorded so `stack merge` knows
        # there is a stack rather than guessing from the pull request list.
        : > "$FORGE/stack"
        for n in $(ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n); do
          [ "$(field "$n" state)" = OPEN ] && echo "$n" >> "$FORGE/stack"
        done
        echo "stack up to date"
        exit 0 ;;
      merge)
        [ -s "$FORGE/stack" ] || { shrug "gh stack merge with nothing linked"; exit 0; }
        # Bottom first. Merging the bottom retargets the rest, which is what
        # makes a stack land as one line rather than as n independent merges.
        for n in $(cat "$FORGE/stack"); do
          [ -f "$PRS/$n" ] || continue
          [ "$(field "$n" state)" = OPEN ] || continue
          "$0" pr merge "$n" --merge >/dev/null || exit 1
        done
        rm -f "$FORGE/stack"
        echo "merged the stack"
        exit 0 ;;
      list|status)
        # What `spoolway stack` reaches for to see the stack before it links one.
        # `status` and `list` are one answer here: both are "what is in the
        # stack right now", and the only thing this double could usefully add
        # for `status` is the linked/unlinked distinction, which the output
        # already carries by way of the state column.
        # Nothing names it explicitly — `spoolway stack` asks on its own — and
        # waved through it answered with an exit 0 and no output at all, which
        # reads as "there is no stack here" to anything that asks. That is the
        # same lie as the others, told about the one thing this step exists to
        # act on.
        #
        # Linked state first, because that is what `merge` will actually walk;
        # otherwise the open task pull requests in the order they were opened,
        # which under a chain is bottom-first by construction.
        listed=""
        if [ -s "$FORGE/stack" ]; then
          listed=$(cat "$FORGE/stack")
        else
          for n in $(ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n); do
            [ "$(field "$n" state)" = OPEN ] || continue
            case "$(field "$n" head)" in task/*) listed="$listed $n" ;; esac
          done
        fi
        if [ -z "$listed" ]; then
          echo "no stack here" >&2
          exit 1
        fi
        for n in $listed; do
          printf '#%s\t%s\t->\t%s\t%s\n' \
            "$n" "$(field "$n" head)" "$(field "$n" base)" "$(field "$n" state)"
        done
        exit 0 ;;
      *)
        shrug "gh stack ${2:-<nothing>}"
        exit 0 ;;
    esac ;;
  repo)
    [ "${2:-}" = view ] && { basename "$ORIGIN" .git; exit 0; }
    shrug "gh repo ${2:-<nothing>}"
    exit 0 ;;
  *)
    shrug "gh ${1:-<nothing>} — not implemented by the sandbox forge"
    exit 0 ;;
esac

verb=${2:-}
shift 2 2>/dev/null || shift $#

case "$verb" in
  create)
    base="" title="" body="" head=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --base)  base=${2:?--base needs a value};  shift 2 ;;
        --title) title=${2:?--title needs a value}; shift 2 ;;
        --body)  body=${2:?--body needs a value};  shift 2 ;;
        --body-file)
          f=${2:?--body-file needs a value}
          body=$(cat "$f") || die "--body-file '$f' could not be read"
          shift 2 ;;
        --head)  head=${2:?--head needs a value};  shift 2 ;;
        --draft|--fill) shift ;;
        *) shrug "unknown flag for pr create: $1"; shift ;;
      esac
    done
    # Without --head, the branch the lane is standing on, exactly as gh does.
    [ -n "$head" ] || head=$(git rev-parse --abbrev-ref HEAD)
    # Defaults rather than refusals: a prompt that phrases the command
    # differently should not be what ends the run.
    [ -n "$base" ]  || { base=main;   shrug "pr create had no --base, assuming $base"; }
    [ -n "$title" ] || { title=$head; shrug "pr create had no --title, using the branch name"; }

    # The one shape that is not a difference of phrasing but a lane that could
    # not work out where its change goes and substituted something rather than
    # saying so. A real `gh` refuses it too — a pull request from a branch to
    # itself has no diff — so this is fidelity rather than strictness, and it
    # turns the mistake into a lane-visible failure on the pass it happens
    # rather than something somebody notices in `prs/1` a day later.
    #
    # Seen in a real run: the versioner opened `head=task/writes-doc` against
    # `base=task/writes-doc` for the first task of a plan, whose base is the
    # plan branch. The stack still merged, which is exactly why it was worth
    # catching here.
    if [ "$head" = "$base" ]; then
      die "pull request from '$head' to itself: a base you could not resolve is a blocker, not a default"
    fi

    # The `pr` step is supposed to push before opening the pull request. If it
    # did not, do it here rather than refuse: a forge that receives a push is
    # what it is standing in for, the run carries on, and the log keeps the
    # fact that the step skipped it. Only a branch that exists nowhere at all
    # is a dead end.
    if ! git -C "$ORIGIN" rev-parse --verify --quiet "refs/heads/$head" >/dev/null; then
      git rev-parse --verify --quiet "refs/heads/$head" >/dev/null \
        || die "branch '$head' exists neither on the remote nor in this checkout"
      shrug "branch '$head' was not pushed before pr create — pushing it now"
      git push -q "$ORIGIN" "refs/heads/$head:refs/heads/$head"
    fi

    # A branch may already have a pull request, and that one is this task's.
    # gh itself refuses a second with "a pull request for branch ... already
    # exists"; `spoolway stack` is told to look before it opens one, and a
    # stand-in that opened a duplicate would make that untestable.
    # Where the two ends of this pull request actually are, right now.
    #
    # Recorded because they are the only description of a task's change that
    # survives the run. Every ref moves afterwards — landing the stack merges
    # each branch into the one below it, so `task/base` ends up a *descendant*
    # of `task/top` — and a diff taken later against a branch name is either
    # empty or backwards. A pull request's two endpoints do not move: they are
    # what it was opened about. scripts/e2e/record.sh reads exactly this.
    head_sha=$(git -C "$ORIGIN" rev-parse --verify --quiet "refs/heads/$head" || true)
    base_sha=$(git -C "$ORIGIN" rev-parse --verify --quiet "refs/heads/$base" || true)

    for existing in $(ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n); do
      if [ "$(field "$existing" head)" = "$head" ] && [ "$(field "$existing" state)" = OPEN ]; then
        note "pr create for '$head', which already has #$existing"
        # A re-run means the branch was force-pushed after a second rebase, and
        # a real forge shows the pull request at its branch's current tip. Move
        # the head with it so the recording is of what was actually handed over,
        # not of the first attempt.
        if [ -n "$head_sha" ] && [ "$head_sha" != "$(field "$existing" head_sha)" ]; then
          sed -i "/^head_sha=/d" "$PRS/$existing"
          echo "head_sha=$head_sha" >> "$PRS/$existing"
        fi
        echo "file://$ORIGIN/pull/$existing"
        exit 0
      fi
    done

    last=$(latest || true)
    n=$(( ${last:-0} + 1 ))
    {
      echo "number=$n"
      echo "base=$base"
      echo "head=$head"
      echo "title=$title"
      echo "state=OPEN"
      echo "head_sha=$head_sha"
      echo "base_sha=$base_sha"
    } > "$PRS/$n"
    printf '%s\n' "$body" > "$PRS/$n.body"
    echo "file://$ORIGIN/pull/$n"
    ;;

  view)
    # `--json` is not an optional nicety here: `spoolway stack` names
    # `gh pr view --json number,url,state` outright, so a double that reads
    # `--json` as a pull request number answers a real lane's question with the
    # newest pull request in plain text. It did, and the lane behaved
    # accordingly — which looked like a bug in `spoolway stack` itself.
    fields="" number=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --json) fields=${2:-}; shift 2 ;;
        --jq|--template) shift 2 ;;
        -*) shrug "unknown flag for pr view: $1"; shift ;;
        *) [ -n "$number" ] || number=$1; shift ;;
      esac
    done
    # "Is there already one for this branch?" is the first thing `spoolway
    # stack` asks, and the answer is very often no — the first lane of every
    # run asks it when the forge is still empty, and every later one asks it
    # about a branch nobody has opened anything for. `gh` says so on stderr
    # and exits 1. That is a normal answer to a normal question, not one of
    # the two genuine dead ends, and it is the answer `spoolway stack` is
    # written to act on: "Only if there is none, `gh pr create`."
    #
    # Real `gh pr view` reads a bare positional three ways: empty means the
    # branch checked out here, all digits means a pull request number, and
    # anything else means a branch name — `spoolway stack` names the branch
    # outright rather than checking one out and asking about "the current
    # one". Handled here rather than in `pr_number`, which `merge` also uses
    # and where a caller naming a branch rather than a number has never been
    # a real shape to expect.
    branch=""
    case "$number" in
      '') branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true) ;;
      *[!0-9]*) branch=$number; number="" ;;
    esac
    if [ -z "$number" ]; then
      if [ -z "$branch" ] || ! n=$(pr_for_branch "$branch"); then
        echo "no pull requests found for branch \"${branch:-HEAD}\"" >&2
        exit 1
      fi
    else
      n=$(pr_number "$number")
    fi
    if [ -z "$fields" ]; then
      printf 'title:\t%s\n' "$(field "$n" title)"
      printf 'state:\t%s\n' "$(field "$n" state)"
      printf 'base:\t%s\n'  "$(field "$n" base)"
      printf 'head:\t%s\n'  "$(field "$n" head)"
    else
      json_for "$n" "$fields"
    fi
    ;;

  list)
    # What `spoolway stack` reaches for to see the stack it is about to link.
    # Only the flags it actually uses: `--json` for machine output, `--state`
    # to ask for the open ones, `--base`/`--head` to narrow.
    fields="" want_state=OPEN filter_base="" filter_head=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --json)  fields=${2:-}; shift 2 ;;
        --state) want_state=$(printf '%s' "${2:-open}" | tr '[:lower:]' '[:upper:]'); shift 2 ;;
        --base)  filter_base=${2:-}; shift 2 ;;
        --head)  filter_head=${2:-}; shift 2 ;;
        --limit) shift 2 ;;
        --jq|--template) shift 2 ;;
        # `--search "head:<branch>"` is how `spoolway stack` asks for one
        # branch's pull request. Waved through, it becomes "list everything" —
        # and a lane that asked about one branch and was handed every branch
        # is being misinformed rather than under-served.
        --search)
          case "${2:-}" in
            head:*) filter_head=${2#head:} ;;
            base:*) filter_base=${2#base:} ;;
            *) shrug "pr list --search '${2:-}' — only head:/base: are understood" ;;
          esac
          shift 2 ;;
        *) shrug "unknown flag for pr list: $1"; shift ;;
      esac
    done

    matching=""
    for n in $(ls -1 "$PRS" 2>/dev/null | grep -xE '[0-9]+' | sort -n); do
      [ "$want_state" = ALL ] || [ "$(field "$n" state)" = "$want_state" ] || continue
      [ -z "$filter_base" ] || [ "$(field "$n" base)" = "$filter_base" ] || continue
      [ -z "$filter_head" ] || [ "$(field "$n" head)" = "$filter_head" ] || continue
      matching="$matching $n"
    done

    if [ -z "$fields" ]; then
      for n in $matching; do
        printf '#%s\t%s\t%s\t%s\n' \
          "$n" "$(field "$n" title)" "$(field "$n" head)" "$(field "$n" state)"
      done
    else
      printf '['
      sep=""
      for n in $matching; do
        printf '%s' "$sep"; json_for "$n" "$fields"; sep=","
      done
      printf ']\n'
    fi
    ;;

  checks)
    # Nothing runs CI here, and a sandbox with no checks configured is not a
    # failing one on a real forge either — so green is the honest answer.
    n=$(pr_number "${1:-}")
    [ "$(field "$n" state)" = OPEN ] || shrug "checks asked about $n, which is $(field "$n" state)"
    echo "All checks were successful"
    ;;

  merge)
    n=$(pr_number "${1:-}")
    [ $# -gt 0 ] && shift
    delete=0
    while [ $# -gt 0 ]; do
      case "$1" in
        --merge) shift ;;
        --delete-branch) delete=1; shift ;;
        # However it was asked for, the merge happens — as a merge commit,
        # GitHub's own default. Deliberately not a squash: the dispatcher's
        # `awaiting` step asks the *real* `gh` about pull requests, which knows
        # nothing of this stand-in, so landing here is detected through git
        # alone — and a squash of a multi-commit branch is exactly the landing
        # git cannot see (src/repo.rs::landed). A merge commit it can.
        --squash|--rebase) shrug "pr merge asked for $1; the sandbox forge makes a merge commit"; shift ;;
        --admin|--yes) shift ;;
        *) shrug "unknown flag for pr merge: $1"; shift ;;
      esac
    done
    # Landing an already-landed pull request is a no-op, not a failure: the
    # merge step re-running is a normal thing for the dispatcher to do.
    if [ "$(field "$n" state)" != OPEN ]; then
      shrug "pull request $n was already $(field "$n" state)"
      echo "Merged pull request #$n into $(field "$n" base)"
      exit 0
    fi

    base=$(field "$n" base)
    head=$(field "$n" head)
    title=$(field "$n" title)

    # A bare repo can host a worktree, and a commit made in one lands in the
    # same object store — so the merge happens inside the forge and the new
    # tip is moved with `update-ref`, rather than pushed back from a clone.
    tmp=$(mktemp -d)
    cleanup() {
      git -C "$ORIGIN" worktree remove --force "$tmp" >/dev/null 2>&1 || true
      rm -rf "$tmp"
    }
    trap cleanup EXIT

    git -C "$ORIGIN" worktree add -q --detach "$tmp" "$base"
    git -C "$tmp" \
      -c user.name="spoolway sandbox forge" \
      -c user.email="forge@spoolway.invalid" \
      merge --no-ff -m "$title (#$n)" "$head" >/dev/null 2>&1 \
      || die "pull request $n does not merge cleanly into $base — rebase it first"
    git -C "$ORIGIN" update-ref "refs/heads/$base" "$(git -C "$tmp" rev-parse HEAD)"

    sed -i 's/^state=OPEN$/state=MERGED/' "$PRS/$n"
    if [ "$delete" = 1 ]; then
      git -C "$ORIGIN" branch -D "$head" >/dev/null 2>&1 || true
    fi
    echo "Merged pull request #$n into $base"
    ;;

  edit)
    # `--base` is why this cannot stay waved through. Retargeting a pull request
    # is a real edit to the thing under test — a lane that opened one against
    # the wrong branch and then corrected it would be told the correction
    # succeeded while the record kept the wrong base, and the stack would read
    # as broken for a reason that had already been fixed.
    n=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --base)  new_base=${2:?--base needs a value};   shift 2 ;;
        --title) new_title=${2:?--title needs a value}; shift 2 ;;
        --body)  new_body=${2:?--body needs a value};   shift 2 ;;
        --add-label|--remove-label|--add-reviewer|--milestone) shift 2 ;;
        -*) shrug "unknown flag for pr edit: $1"; shift ;;
        *) [ -n "$n" ] || n=$1; shift ;;
      esac
    done
    n=$(pr_number "$n")

    if [ -n "${new_base:-}" ]; then
      sed -i "s|^base=.*|base=${new_base}|" "$PRS/$n"
      # The base moved, so what this pull request is *about* moved with it. Kept
      # in step for scripts/e2e/record.sh, which diffs these two endpoints.
      new_base_sha=$(git -C "$ORIGIN" rev-parse --verify --quiet "refs/heads/$new_base" || true)
      if [ -n "$new_base_sha" ]; then
        sed -i "/^base_sha=/d" "$PRS/$n"
        echo "base_sha=$new_base_sha" >> "$PRS/$n"
      fi
    fi
    [ -n "${new_title:-}" ] && sed -i "s|^title=.*|title=${new_title}|" "$PRS/$n"
    [ -n "${new_body:-}" ] && printf '%s\n' "$new_body" > "$PRS/$n.body"

    echo "file://$ORIGIN/pull/$n"
    ;;

  # `pr close`, `pr comment`, anything else a prompt might grow: recorded and
  # waved through, never the reason a run stops short of `done`.
  *) shrug "gh pr ${verb:-<nothing>} — not implemented by the sandbox forge" ;;
esac
