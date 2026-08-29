# Written once by `spoolway init`. Yours after that; `spoolway update`
# never touches it. Same branches as jira.sh, same environment.
#
# Every `acli` command below was checked against acli 1.3.30-stable. Three
# things need a site to confirm, and each is a one-line edit if yours
# differs: that `workitem create --json` names the new key `.key`, that your
# project spells its link type `Blocks` and its epic status `Done`, and — for
# `fetch`, below — that `workitem view --json` names the summary, status and
# label fields the way this reads them; run it once by hand against a real
# key and adjust the filter to match if it does not.
#
# The task file itself is deliberately not sent — see jira.sh's header for
# why. The comment names the task and leaves the file in the queue.
#
# A create call that comes back with no key — `acli` failing, the JSON shape
# changing underfoot — exits loudly rather than writing an empty
# `epic=`/`ticket=` line: a lost key is a ticket nothing ever links to again,
# and this codebase's error style has no room for losing that quietly.

$project = $env:SPOOLWAY_PROJECT_KEY      # `[issue_tracking] project_key`

# Hangs $child (an issue this hook just created, by its own key) as a Jira
# link under $parent, doing nothing at all unless $parent is a
# `.../browse/<key>` URL for this same $project. See jira.sh's own comment
# on `hang_under` — same job, same "Relates" guess at a link type.
function Hang-Under($parent, $child) {
  if (-not $child) { return }
  if ($parent -notmatch "^https://[^/]+/browse/$([regex]::Escape($project))-\d+$") { return }
  acli jira workitem link create --out $parent.Split('/')[-1] --in $child --type Relates --yes
}

if ($env:SPOOLWAY_EVENT -eq 'fetch') {
  $fields = acli jira workitem view --key $env:SPOOLWAY_REF --json | ConvertFrom-Json
  if (-not $fields) {
    Write-Error "jira.ps1: acli returned nothing for `"$env:SPOOLWAY_REF`" — does the key exist?"
    exit 1
  }
  $comments = acli jira workitem comment list --key $env:SPOOLWAY_REF --json | ConvertFrom-Json
  $site = [regex]::Match($fields.self, '^https://[^/]+').Value
  [pscustomobject]@{
    ref      = $fields.key
    url      = "$site/browse/$($fields.key)"
    title    = $fields.summary
    state    = "$($fields.status.name ?? $fields.status)".ToLower()
    labels   = @($fields.labels)
    body     = $fields.description
    comments = @($comments | ForEach-Object { [pscustomobject]@{
      author = "$($_.author.name ?? $_.author)"
      body   = $_.body
    } })
  } | ConvertTo-Json -Depth 5 | Set-Content $env:SPOOLWAY_OUT
  exit 0
}

if ($env:SPOOLWAY_EVENT -eq 'open') {
  $epic = $env:SPOOLWAY_EPIC               # set when the group already names one
  if (-not $epic -and [int]$env:SPOOLWAY_GROUP_SIZE -gt 1) {
    $epic = (acli jira workitem create --project $project --type Epic `
               --summary $env:SPOOLWAY_GROUP `
               --description-file $env:SPOOLWAY_EPIC_BODY --json | ConvertFrom-Json).key
    if (-not $epic) {
      Write-Error 'jira.ps1: acli returned no epic key — did "workitem create" succeed?'
      exit 1
    }
    Hang-Under $env:SPOOLWAY_SOURCE $epic
  }
  $parent = if ($epic) { @('--parent', $epic) } else { @() }
  $ticket = (acli jira workitem create --project $project --type Story `
               --summary $env:SPOOLWAY_TITLE `
               --description-file $env:SPOOLWAY_TICKET_BODY @parent --json |
             ConvertFrom-Json).key
  if (-not $ticket) {
    Write-Error 'jira.ps1: acli returned no ticket key — did "workitem create" succeed?'
    exit 1
  }
  if (-not $epic) { Hang-Under $env:SPOOLWAY_SOURCE $ticket }  # a group of one has no epic to hang under
  # Jira has real issue links, unlike GitHub's bare sentence in the body.
  foreach ($dep in ($env:SPOOLWAY_DEPENDS_TICKETS -split '\s+' | Where-Object { $_ })) {
    acli jira workitem link create --out $dep --in $ticket --type Blocks --yes
  }
  "epic=$epic", "ticket=$ticket" | Set-Content $env:SPOOLWAY_OUT
  exit 0
}

if ($env:SPOOLWAY_EVENT -eq 'done' -and $env:SPOOLWAY_GROUP_LAST -and $env:SPOOLWAY_EPIC) {
  acli jira workitem transition --key $env:SPOOLWAY_EPIC --status Done --yes
}

if ($env:SPOOLWAY_EVENT -notin 'blocked', 'paused') { exit 0 }

acli jira workitem comment create --key $env:SPOOLWAY_TICKET --body @"
spoolway - $env:SPOOLWAY_TASK is $env:SPOOLWAY_EVENT at $env:SPOOLWAY_FROM
"@
