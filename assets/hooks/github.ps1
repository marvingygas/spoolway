# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Same branches as github.sh, same environment.
#
# Every command here was run against a real repository with gh 2.97.0, in
# its `github.sh` form, except the two that write: `gh pr edit` and the
# `done` branch's own `gh issue comment`. See that file's header. This
# script itself was run under Windows PowerShell 5.1 with a stub `gh` on
# `PATH`, which is how the byte order mark below turned out to matter, and
# separately how `gh pr edit`'s own call came to write a temporary file
# rather than pipe the body straight into `gh`'s stdin: 5.1 encodes a piped
# string as `$OutputEncoding`, plain ASCII by default, which would silently
# mangle any non-ASCII character the body holds. The `[Console]::
# OutputEncoding` assignment below this header, the fix for a different but
# related encoding gap on the *read* side, was found the same way but has
# not itself been re-run live: nothing in this file has changed which real
# `gh` calls this script makes, only how their answers get decoded. The
# REST sub-issue endpoint wants the issue's integer id, not the node id
# `gh issue view --json id` returns.
#
# This file starts with a UTF-8 byte order mark, and has to keep it.
# `platform::powershell` falls back to Windows PowerShell 5.1 wherever pwsh
# is not installed, and 5.1 reads a script with no mark as the machine's
# ANSI code page rather than UTF-8. On a Western European one that turns
# every em dash below into three characters ending in a right double quote,
# which PowerShell accepts as a string delimiter — so the `done` branch's
# messages below would close their own string literals and the whole script
# would fail to parse. The mark makes 5.1 decode the file as what it is.

$repo = $env:SPOOLWAY_PROJECT_KEY         # `[issue_tracking] project_key`

# Windows PowerShell 5.1 decodes a captured native command's stdout using
# [Console]::OutputEncoding, the console's OEM code page by default — not
# UTF-8. `gh` always emits UTF-8 regardless of that code page, so every
# non-ASCII byte in an issue title, a comment or a pull request body would
# otherwise come back garbled the moment it is *captured*, not only when it
# is written back out (the bug the `done` branch's own `--body-file`
# already works around). Setting this once, before the first `gh` call
# below, fixes every one of them.
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

# Hangs $child (an issue this hook just created — the URL `gh issue create`
# printed) as a GitHub sub-issue under $parent, doing nothing at all unless
# $parent ends in `/<repo>/issues/<n>` for this same $repo — matched by the
# tail of the URL rather than a fixed `github.com` host, so this reads a
# GitHub Enterprise Server issue the same way. See github.sh's own comment
# on `hang_under` — same job, same reuse of the `sub_issues` call.
function Hang-Under($parent, $child) {
  if (-not $child) { return }
  if ($parent -notmatch "/$([regex]::Escape($repo))/issues/\d+$") { return }
  $n = $parent.Split('/')[-1]
  $id = gh api "repos/$repo/issues/$($child.Split('/')[-1])" -q .id
  gh api -X POST "repos/$repo/issues/$n/sub_issues" -F sub_issue_id=$id
}

if ($env:SPOOLWAY_EVENT -eq 'fetch') {
  $filter = '{ref: (.number | tostring), url, title, state: (.state | ascii_downcase), ' +
            'labels: [.labels[].name], body, ' +
            'comments: [.comments[] | {author: .author.login, body}]}'
  gh issue view $env:SPOOLWAY_REF -R $repo `
    --json number,url,title,state,labels,body,comments --jq $filter |
    Set-Content $env:SPOOLWAY_OUT
  exit 0
}

if ($env:SPOOLWAY_EVENT -eq 'open') {
  $epic = $env:SPOOLWAY_EPIC              # set when the group already names one
  if (-not $epic -and [int]$env:SPOOLWAY_GROUP_SIZE -gt 1) {
    $epic = gh issue create -R $repo -t $env:SPOOLWAY_GROUP `
                            -F $env:SPOOLWAY_EPIC_BODY
    Hang-Under $env:SPOOLWAY_SOURCE $epic
  }
  $ticket = gh issue create -R $repo -t $env:SPOOLWAY_TITLE `
                            -F $env:SPOOLWAY_TICKET_BODY
  if ($epic) {                            # gh has no sub-issue command
    $id = gh api "repos/$repo/issues/$($ticket.Split('/')[-1])" -q .id
    gh api -X POST "repos/$repo/issues/$($epic.Split('/')[-1])/sub_issues" `
      -F sub_issue_id=$id
  } else {
    Hang-Under $env:SPOOLWAY_SOURCE $ticket  # a group of one has no epic to hang under
  }
  # The short handle spoolway puts in generated names, and the issue's web
  # address kept on the task for later use — spoolway stores and validates
  # url= but shows it nowhere yet. $epic/$ticket are already URLs: take the
  # issue number off the end, with a `gh-` prefix so the slug starts with a
  # letter. The epic keys a group, the ticket keys a group of one that never
  # opened an epic.
  $key = if ($epic) { $epic } else { $ticket }
  "epic=$epic", "ticket=$ticket", "slug=gh-$($key.Split('/')[-1])", "url=$key" |
    Set-Content $env:SPOOLWAY_OUT
  exit 0
}

# See github.sh's own comment on this branch for the full explanation,
# including why a workflow triggered on a push to the default branch cannot
# close the ticket (the trailer lives only in the pull request's own body,
# never in a commit) and why the recovery text below never points at a
# queue command: done is not a step a person can advance a task past by
# hand (see retry_if_failed's own doc in tracking.rs) — a task held here on
# issue_tracking.on_fail = pause retries this same hook automatically on
# the dispatcher's next pass, no command needed. The shipped default,
# on_fail = ignore, archives the task on a failed hook exactly like a
# passing one instead, so every message also gives the manual fix.
if ($env:SPOOLWAY_EVENT -eq 'done' -and $env:SPOOLWAY_TICKET) {
  $ticketN = $env:SPOOLWAY_TICKET.Split('/')[-1]
  $pr = gh pr view $env:SPOOLWAY_BRANCH -R $repo --json url --jq .url
  if ($LASTEXITCODE -ne 0) {
    Write-Error "spoolway: gh pr view failed for $env:SPOOLWAY_BRANCH - check that gh is logged in to $repo. A task held on issue_tracking.on_fail = pause retries this automatically once that is fixed; the default on_fail = ignore already moved this task on, so open $env:SPOOLWAY_TICKET's pull request yourself and add Closes #$ticketN to its body."
    exit 1
  }
  if (-not $pr) {
    Write-Error "spoolway: gh pr view found no open pull request for $env:SPOOLWAY_BRANCH - open one (or re-run spoolway stack). A task held on issue_tracking.on_fail = pause retries this automatically once one exists; otherwise add Closes #$ticketN to the new pull request's body yourself."
    exit 1
  }
  # `gh`'s own stdout comes back from PowerShell capture as one array
  # element per line rather than one string — assigning it to `$prBody`
  # directly would make `-notmatch` search line by line and would flatten
  # every real newline to a single space on interpolation below. `Out-String`
  # joins it back into the one scalar github.sh's own `$(...)` already gets.
  $prBody = (gh pr view $pr --json body --jq .body | Out-String).TrimEnd("`r`n")
  if ($LASTEXITCODE -ne 0) {
    Write-Error "spoolway: could not read $pr's body - check gh access to $repo; nothing was changed. A task held on issue_tracking.on_fail = pause retries this automatically once that is fixed; otherwise add Closes #$ticketN to $pr's body yourself."
    exit 1
  }
  # A plain `Closes #<n>` search would read ticket #1 as already linked
  # inside someone else's `Closes #123` — the lookahead below only matches
  # when the number ends exactly there.
  if ($prBody -notmatch "Closes #$([regex]::Escape($ticketN))(?!\d)") {
    $trailer = "`n`nCloses #$ticketN`n"
    $content = "$prBody$trailer"
    # GitHub's 65,536-byte ceiling is bytes, not the UTF-16 code units
    # `.Length` counts, so the check below has to go through UTF-8 byte
    # counts explicitly — the same reason github.sh reaches for `wc -c`
    # rather than `${#var}`.
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    if ($utf8NoBom.GetByteCount($content) -gt 65536) {
      # Cutting the existing text to make room would risk truncating
      # github.sh's own trailer — its conflict/touches list and co-author
      # tag — so this fails loudly rather than silently corrupting the pull
      # request.
      Write-Error "spoolway: $pr's body is already at GitHub's 65,536-byte limit - trim it by hand so Closes #$ticketN fits. A task held on issue_tracking.on_fail = pause retries this automatically afterward; otherwise add the trailer yourself."
      exit 1
    }
    # Windows PowerShell 5.1 encodes a native command's own stdin as
    # `$OutputEncoding` — plain ASCII by default — when a string is piped to
    # it directly, silently mangling any non-ASCII character the body holds
    # (an em dash, say, the same mark this project's own prose uses). A file
    # written with an explicit encoding sidesteps that pipeline entirely:
    # `gh` reads exactly the UTF-8 bytes this wrote, no more and no fewer,
    # which is also what makes the byte count above the real one `gh` sees
    # rather than an estimate a pipe might still inflate.
    $bodyFile = [System.IO.Path]::GetTempFileName()
    try {
      [System.IO.File]::WriteAllText($bodyFile, $content, $utf8NoBom)
      gh pr edit $pr --body-file $bodyFile
      if ($LASTEXITCODE -ne 0) {
        # A failed edit leaves the pull request unlinked — reporting success
        # anyway (the comment below) would claim a handoff that never happened.
        Write-Error "spoolway: gh pr edit failed while handing $env:SPOOLWAY_TICKET off to $pr - check gh access to $repo; the pull request's body was not changed. A task held on issue_tracking.on_fail = pause retries this automatically once that is fixed; otherwise add Closes #$ticketN to $pr's body yourself."
        exit 1
      }
    } finally {
      Remove-Item $bodyFile -ErrorAction SilentlyContinue
    }
  }
  gh issue comment $env:SPOOLWAY_TICKET `
    --body "spoolway: handed off to $pr — awaiting merge and whatever merge-time closure this repository has configured."
  if ($LASTEXITCODE -ne 0) {
    Write-Error "spoolway: gh issue comment failed on $env:SPOOLWAY_TICKET after handing it off to $pr - $pr already links $env:SPOOLWAY_TICKET, so only the comment is missing. Check gh access to $repo. A task held on issue_tracking.on_fail = pause retries this automatically, redoing nothing since the link already exists; otherwise comment on $env:SPOOLWAY_TICKET yourself."
    exit 1
  }
}

if ($env:SPOOLWAY_EVENT -notin 'blocked', 'paused') { exit 0 }

$body = @"
**spoolway** — ``$env:SPOOLWAY_TASK`` is **$env:SPOOLWAY_EVENT** at ``$env:SPOOLWAY_FROM``

<details><summary>Task file</summary>

``````markdown
$(Get-Content $env:SPOOLWAY_TASK_FILE -Raw -TotalCount 50000)
``````
</details>
"@
$body | gh issue comment $env:SPOOLWAY_TICKET --body-file -
