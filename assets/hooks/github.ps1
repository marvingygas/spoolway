# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Same branches as github.sh, same environment.
#
# Every command here was run against a real repository with gh 2.97.0, in
# its `github.sh` form. The REST sub-issue endpoint wants the issue's
# integer id, not the node id `gh issue view --json id` returns.

$repo = $env:SPOOLWAY_PROJECT_KEY         # `[issue_tracking] project_key`

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
  "epic=$epic", "ticket=$ticket" | Set-Content $env:SPOOLWAY_OUT
  exit 0
}

if ($env:SPOOLWAY_EVENT -eq 'done' -and $env:SPOOLWAY_GROUP_LAST -and $env:SPOOLWAY_EPIC) {
  gh issue close $env:SPOOLWAY_EPIC -c 'spoolway: every task here is done.'
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
