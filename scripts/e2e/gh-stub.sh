#!/usr/bin/env bash
# A minimal `gh` double, shared by `scripts/e2e/suites/stack.sh`,
# `stacking.sh`, and the `github.sh` hook coverage in
# `scripts/e2e/suites/issue-tracking.sh` — not the fuller `e2e-fake-gh.sh`
# every other suite shares, because that one speaks
# `pr create --body` and `spoolway stack` speaks `--body-file`, and these
# suites are explicitly the ones with no real forge to talk to: local git
# plumbing and this script are the whole of what `spoolway stack` and the
# shipped `github.sh` hook reach out to.
#
# Pull requests are flat files under $GH_STUB_PRS, one per number: `base=`,
# `head=` and `title=` lines, plus a `<n>.body` file holding the body exactly
# as `--body-file` handed it over, and `<n>.comment` the last comment `gh pr
# comment` posted (`--body` or `--body-file -`, either form). $GH_STUB_URL is
# the `file://…` prefix a `gh pr view --json url` answer is built from.
#
# Issues are the same shape, under $GH_STUB_ISSUES: `<n>` holds `repo=` and
# `title=`, `<n>.body` is the file `-F` named, `<n>.comment` is the last
# comment posted (`--body-file -` reads it off stdin), and `<n>.closed`
# exists once `gh issue close` has run. `<n>.labels` is a plain list, one
# label per line, seeded by `issue create --label` and mutated in place by
# `issue edit --add-label`/`--remove-label` — the marker-and-label design's
# own way of tracking status, since GitHub issues have no state past
# open/closed. `<n>.parent` and `<n>.blocked_by` hold `issue create`'s own
# `--parent`/`--blocked-by` value, when either was given. `<n>.labels.json`
# and `<n>.comments.json`, in real `gh --json`'s own shape —
# `[{"name":...}]` and `[{"author":{"login":...},"body":...}]` — are not
# written by anything this stub does; a suite seeds them by hand to stand in
# for an issue a person already filed, with labels and comments on it before
# `fetch` ever runs. Both read back as `[]` when absent.
set -euo pipefail

PRS=${GH_STUB_PRS:?GH_STUB_PRS must name where pull requests live}
mkdir -p "$PRS"

ISSUES=${GH_STUB_ISSUES:-$PRS/../issues}
mkdir -p "$ISSUES"

next_issue() {
  local n=0 f b
  for f in "$ISSUES"/[0-9]*; do
    [ -e "$f" ] || continue
    b=$(basename "$f")
    case "$b" in *.*) continue ;; esac
    [ "$b" -gt "$n" ] && n=$b
  done
  echo $((n + 1))
}

# A stack, once one exists, is a flat file of pull request numbers, bottom to
# top — `$STACKS/<n>` for stack `<n>`, one line per member in the order it
# joined. Kept beside `$PRS` rather than under `GH_STUB_PRS` itself, so a
# glob over pull request numbers there never trips over a stack file.
STACKS=${GH_STUB_STACKS:-$PRS/../stacks}
mkdir -p "$STACKS"

field() { sed -n "s/^$2=//p" "$PRS/$1" 2>/dev/null; }
issue_field() { sed -n "s/^$2=//p" "$ISSUES/$1" 2>/dev/null; }

# The stack number a pull request already belongs to, or nothing.
stack_of() {
  local pr=$1 f
  for f in "$STACKS"/[0-9]*; do
    [ -e "$f" ] || continue
    grep -qx "$pr" "$f" && { basename "$f"; return 0; }
  done
  return 1
}

next_number() {
  local n=0 f b
  for f in "$PRS"/[0-9]*; do
    [ -e "$f" ] || continue
    b=$(basename "$f")
    case "$b" in *.body) continue ;; esac
    [ "$b" -gt "$n" ] && n=$b
  done
  echo $((n + 1))
}

pr_for_branch() {
  local branch=$1 f n
  for f in "$PRS"/[0-9]*; do
    [ -e "$f" ] || continue
    case "$f" in *.body) continue ;; esac
    n=$(basename "$f")
    [ "$(field "$n" head)" = "$branch" ] && { echo "$n"; return 0; }
  done
  return 1
}

case "${1:-}" in
  # `spoolway doctor` reads this back to enforce github.sh's own `#
  # spoolway-requires: gh >= 2.97.0` line — answered at that floor, so a
  # doctor run against this double reads the same as one against the real,
  # tested `gh`.
  --version) echo "gh version 2.97.0 (2024-06-03)"; exit 0 ;;
  auth) exit 0 ;;
  pr)
    case "${2:-}" in
      checks)
        # A caller invokes this with no target at all — `gh pr checks
        # --watch --fail-fast` — so, like real `gh`, the pull request is
        # whichever one belongs to the current branch. Never asked to
        # actually watch anything real: this sandbox forge runs no CI, so a
        # pull request that exists just gets the same shrug `e2e-fake-gh.sh`'s
        # own `checks` case gives.
        branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
        pr_for_branch "$branch" >/dev/null || {
          echo "no pull requests found for branch \"$branch\"" >&2
          exit 1
        }
        echo "All checks were successful"
        ;;
      view)
        # `--jq` filters the raw shape below for real, the same way `issue
        # view` already does — `github.sh`'s `done` branch asks for
        # `--json url --jq .url` to unwrap a bare string rather than the
        # whole object, and `-r` is what strips the quotes `jq` would
        # otherwise wrap a string result in, matching real `gh`'s own
        # `--jq` output.
        branch=$3
        shift 3
        jqf=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --jq) jqf=$2; shift 2 ;;
            *) shift ;;
          esac
        done
        n=$(pr_for_branch "$branch") || {
          echo "no pull requests found for branch \"$branch\"" >&2
          exit 1
        }
        raw=$(printf '{"number":%s,"url":"%s/pull/%s","state":"OPEN"}\n' \
          "$n" "${GH_STUB_URL:-file:///origin}" "$n")
        if [ -n "$jqf" ]; then
          echo "$raw" | jq -r "$jqf"
        else
          echo "$raw"
        fi
        ;;
      create)
        shift 2
        base="" head="" title="" bodyfile=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --base) base=$2; shift 2 ;;
            --head) head=$2; shift 2 ;;
            --title) title=$2; shift 2 ;;
            --body-file) bodyfile=$2; shift 2 ;;
            *) shift ;;
          esac
        done
        if n=$(pr_for_branch "$head"); then
          echo "a pull request for branch \"$head\" already exists" >&2
          exit 1
        fi
        n=$(next_number)
        {
          echo "base=$base"
          echo "head=$head"
          echo "title=$title"
        } > "$PRS/$n"
        cp "$bodyfile" "$PRS/$n.body"
        printf '%s/pull/%s\n' "${GH_STUB_URL:-file:///origin}" "$n"
        ;;
      comment)
        # `gh pr comment <url-or-number> -R <repo> --body <text>` — the
        # target is the third word, same rule `issue comment` below
        # follows. `github.sh`'s marker comment always uses `--body`, never
        # `--body-file`, so this only needs the one form — but both are
        # accepted, the same atomic write-then-rename `issue comment`
        # already uses so a watcher never reads a half-written file.
        target=$3
        shift 3
        bodyfile="" body=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --body) body=$2; shift 2 ;;
            --body-file) bodyfile=$2; shift 2 ;;
            *) shift ;;
          esac
        done
        n=${target##*/}
        if [ -n "$bodyfile" ]; then
          if [ "$bodyfile" = "-" ]; then
            cat > "$PRS/$n.comment.part"
          else
            cp "$bodyfile" "$PRS/$n.comment.part"
          fi
        else
          printf '%s' "$body" > "$PRS/$n.comment.part"
        fi
        mv "$PRS/$n.comment.part" "$PRS/$n.comment"
        ;;
      *) echo "gh pr ${2:-<nothing>}: not implemented by this stub" >&2; exit 1 ;;
    esac
    ;;
  issue)
    case "${2:-}" in
      create)
        shift 2
        repo="" title="" bodyfile="" parent="" blocked_by="" labels=""
        while [ $# -gt 0 ]; do
          case "$1" in
            -R) repo=$2; shift 2 ;;
            -t) title=$2; shift 2 ;;
            -F) bodyfile=$2; shift 2 ;;
            --parent) parent=$2; shift 2 ;;
            --blocked-by) blocked_by=$2; shift 2 ;;
            # Repeatable, the way real `gh issue create` accepts it — one
            # call names `spoolway:task` alone, but nothing stops a project
            # from having its own labels alongside it someday.
            --label) labels="$labels${labels:+ }$2"; shift 2 ;;
            *) shift ;;
          esac
        done
        n=$(next_issue)
        { echo "repo=$repo"; echo "title=$title"; } > "$ISSUES/$n"
        cp "$bodyfile" "$ISSUES/$n.body"
        # Plain text, one flag's own value per file — not the pre-seeded
        # `.labels.json` shape `fetch`'s own tests write by hand for an
        # issue spoolway never created; `issue edit` below reads and
        # rewrites this same file for the labels it adds or removes.
        [ -n "$parent" ] && echo "$parent" > "$ISSUES/$n.parent"
        [ -n "$blocked_by" ] && echo "$blocked_by" > "$ISSUES/$n.blocked_by"
        for label in $labels; do echo "$label"; done > "$ISSUES/$n.labels"
        printf '%s/%s/issues/%s\n' "${GH_STUB_URL:-file:///origin}" "$repo" "$n"
        ;;
      edit)
        # `gh issue edit <ref> -R <repo> --remove-label X --add-label Y` —
        # `mark_in_progress` and `hand_off_for_review`'s label swap, the
        # marker-and-label design's own way of tracking a ticket's status
        # since GitHub issues have no state past open/closed. Applied in
        # call order, remove before add, the same order `github.sh` itself
        # always passes them in.
        shift 2
        ref=$1
        shift
        while [ $# -gt 0 ]; do
          case "$1" in
            -R) shift 2 ;;
            --remove-label)
              n=${ref##*/}
              touch "$ISSUES/$n.labels"
              grep -vFx "$2" "$ISSUES/$n.labels" > "$ISSUES/$n.labels.tmp" 2>/dev/null || true
              mv "$ISSUES/$n.labels.tmp" "$ISSUES/$n.labels"
              shift 2
              ;;
            --add-label)
              n=${ref##*/}
              touch "$ISSUES/$n.labels"
              grep -qFx "$2" "$ISSUES/$n.labels" 2>/dev/null || echo "$2" >> "$ISSUES/$n.labels"
              shift 2
              ;;
            *) shift ;;
          esac
        done
        ;;
      view)
        # Two shapes share this one case: `gh issue view <ref> -R <repo>
        # --json id -q .id` once asked for GitHub's *node* id, which the
        # sub-issue endpoint refuses — `github.sh` asks `gh api` for the
        # numeric id instead now, but this is kept answering it for a
        # hand-edited hook that still calls it. `fetch`'s own call asks for
        # every field at once, `--json number,url,title,state,labels,body,
        # comments --jq '<filter>'`, and gets them built from the flat files
        # under $GH_STUB_ISSUES, with the caller's own `--jq` filter run for
        # real — this stub stands in for `gh`, not for `jq`, and github.sh's
        # filter is exactly what turns the raw shape below into fetch's own.
        ref=$3
        shift 3
        repo="" json="" jqf=""
        while [ $# -gt 0 ]; do
          case "$1" in
            -R|--repo) repo=$2; shift 2 ;;
            --json) json=$2; shift 2 ;;
            --jq) jqf=$2; shift 2 ;;
            -q) jqf=$2; shift 2 ;;
            *) shift ;;
          esac
        done
        n=${ref##*/}
        if [ "$json" = id ]; then
          echo "$n"
        else
          state=OPEN
          [ -f "$ISSUES/$n.closed" ] && state=CLOSED
          raw=$(jq -n \
            --arg number "$n" \
            --arg url "${GH_STUB_URL:-file:///origin}/$repo/issues/$n" \
            --arg title "$(issue_field "$n" title)" \
            --arg state "$state" \
            --argjson labels "$(cat "$ISSUES/$n.labels.json" 2>/dev/null || echo '[]')" \
            --arg body "$(cat "$ISSUES/$n.body" 2>/dev/null || true)" \
            --argjson comments "$(cat "$ISSUES/$n.comments.json" 2>/dev/null || echo '[]')" \
            '{number: ($number | tonumber), url: $url, title: $title, state: $state,
              labels: $labels, body: $body, comments: $comments}')
          if [ -n "$jqf" ]; then
            echo "$raw" | jq -c "$jqf"
          else
            echo "$raw"
          fi
        fi
        ;;
      comment)
        # `gh issue comment <url-or-number> --body-file -` — the target is the
        # third word, not the second: `issue` and `comment` are both consumed
        # before it.
        url=$3
        shift 3
        bodyfile=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --body-file) bodyfile=$2; shift 2 ;;
            *) shift ;;
          esac
        done
        n=${url##*/}
        # Written to a sibling and renamed into place, never straight to the
        # name a suite watches. A hook posting a comment runs detached, so a
        # suite polling for `<n>.comment` to exist is racing this write, and a
        # plain `cat >` creates the file empty and fills it afterwards — the
        # poll breaks on the empty one and the assertion reads nothing. A
        # rename within one directory is atomic, so the watcher sees either no
        # file or the whole comment.
        if [ "$bodyfile" = "-" ]; then
          cat > "$ISSUES/$n.comment.part"
        else
          cp "$bodyfile" "$ISSUES/$n.comment.part"
        fi
        mv "$ISSUES/$n.comment.part" "$ISSUES/$n.comment"
        ;;
      close)
        # Third word again — see `comment` above.
        url=$3
        : > "$ISSUES/${url##*/}.closed"
        ;;
      *) echo "gh issue ${2:-<nothing>}: not implemented by this stub" >&2; exit 1 ;;
    esac
    ;;
  api)
    # Real `gh` takes its flags in any order, so `gh api -X POST <path>` and
    # `gh api <path> -X POST` are the same call. One pass over the lot: the
    # path is whichever word is not a flag or a flag's own value.
    shift
    path="" query="" field=""
    while [ $# -gt 0 ]; do
      case "$1" in
        -q|--jq) query=$2; shift 2 ;;
        -F|--field|-f|--raw-field) field=$2; shift 2 ;;
        -X|--method|-H|--header|--input) shift 2 ;;
        -*) shift ;;
        *) path=$1; shift ;;
      esac
    done
    case "$path" in
      */issues/*/sub_issues)
        # `POST repos/<repo>/issues/<n>/sub_issues -F sub_issue_id=<id>` —
        # the endpoint github.sh reaches for because `gh` 2.97.0 has no
        # sub-issue command of its own. Recorded rather than actually
        # linked anywhere: nothing else in this suite reads it back, so the
        # file existing at all is what "the call was made" means here.
        n=${path%/sub_issues}; n=${n##*/}
        echo "$field" >> "$ISSUES/$n.sub_issues"
        exit 0
        ;;
      */issues/[0-9]*)
        # `gh api repos/<repo>/issues/<n> -q .id`: the issue's numeric id, the
        # only thing the sub-issue endpoint accepts. The stub's issue number
        # stands in for it.
        echo "${path##*/}"
        exit 0
        ;;
      */stacks/*/add)
        # Joining an existing stack. A real stack is one linear chain, so this
        # only succeeds when the new member's own `base` is the branch the
        # stack's current top pull request has as its `head` — anything else
        # is the same fork GitHub's real API refuses with HTTP 422.
        n=${path%/add}; n=${n##*/stacks/}
        new_pr=$(cat | grep -oE '[0-9]+' | tail -1)
        top_pr=$(tail -1 "$STACKS/$n" 2>/dev/null || true)
        if [ -n "$top_pr" ] && [ "$(field "$new_pr" base)" != "$(field "$top_pr" head)" ]; then
          echo "gh: Pull requests must form a stack, where each PR's base ref is the previous PR's head ref (HTTP 422)" >&2
          exit 1
        fi
        echo "$new_pr" >> "$STACKS/$n"
        exit 0
        ;;
      */stacks)
        # Creating a stack: `{"pull_requests":[<bottom>,<top>]}`, recorded in
        # the order given — bottom first, by construction of the caller.
        n=1
        for f in "$STACKS"/[0-9]*; do
          [ -e "$f" ] || continue
          b=$(basename "$f")
          [ "$b" -ge "$n" ] && n=$((b + 1))
        done
        cat | grep -oE '[0-9]+' > "$STACKS/$n"
        exit 0
        ;;
      */stacks/*)
        # `-q '.pull_requests[-1] | .number, .head.ref'`: the stack's own top,
        # as two lines — `stack_top_head` in `src/commands/stack.rs` reads
        # exactly this shape.
        n=${path##*/stacks/}
        top_pr=$(tail -1 "$STACKS/$n" 2>/dev/null || true)
        [ -n "$top_pr" ] || exit 1
        printf '%s\n%s\n' "$top_pr" "$(field "$top_pr" head)"
        exit 0
        ;;
      *)
        # `-q .stack.number` on a single pull request.
        pr=${path##*/pulls/}
        if [ "$query" = ".stack.number" ] && n=$(stack_of "$pr"); then
          echo "$n"
          exit 0
        fi
        exit 1
        ;;
    esac
    ;;
  *) echo "gh ${1:-<nothing>}: not implemented by this stub" >&2; exit 1 ;;
esac
