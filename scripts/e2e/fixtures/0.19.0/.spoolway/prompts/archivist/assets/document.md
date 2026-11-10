---
domain: <short-name>
covers:
  - "src/<area>/**"
---

<!--
  The starting structure for a domain document.

  Keep the headings this domain needs, delete the ones it has no answer for,
  and add your own where it needs them. Only the frontmatter is fixed: `domain`
  names the document, `covers` is the glob that routes future changes back to
  it. Keep `covers` matching where the code lives.
-->

# <Domain>

<One sentence: what this part of the system is for, in words a newcomer would use.>

## Overview

What this domain is for, and who uses it. One short paragraph.

Describe it as it is today, not what it used to be or what a plan proposed.

## How it works

The path through the domain, end to end.

Draw a Mermaid diagram for any flow, state machine or lifecycle with more
than two steps. Put a screenshot of every screen this domain has, from
`docs/screenshots/`, next to the text that describes it.

## Key concepts

The terms this domain uses that a reader would otherwise have to infer.

| Term | What it means here |
| --- | --- |

## Usage

The smallest thing that actually runs. One complete example beats three partial
ones.

## Configuration

The settings that change this domain's behaviour, and what each one decides.

| Setting | Default | What it controls |
| --- | --- | --- |

## Reference

Where things live, so a reader can get from this page to the code.

| Path | Responsibility |
| --- | --- |

## Constraints

Only the limits a person hits while using the domain. Internal rules and edge
cases stay in the code.

## Related

The neighbouring domains, and how this one connects to them.
