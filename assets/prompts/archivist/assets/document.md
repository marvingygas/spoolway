---
domain: <short-name>
covers:
  - "src/<area>/**"
---

<!--
  The starting structure for a domain document.

  Keep the headings that earn their place, delete the ones this domain has no
  answer for, and add your own where it needs them. Only the frontmatter is
  fixed: `domain` names the document, `covers` is the glob that routes future
  changes back to it. A `covers` that no longer matches where the code lives is
  how a document starts rotting.
-->

# <Domain>

<One sentence: what this part of the system is for, in words a newcomer would use.>

## Overview

What this domain is for, the boundary it owns, and who calls it. Two or three
paragraphs someone could read before opening a single file.

Describe it as it is today, not what it used to be or what a plan proposed.

## How it works

The path through the domain, end to end.

Reach for a diagram only when a flow, a state machine, or a lifecycle is
genuinely hard to say in a paragraph — a picture of two boxes is worth less than
the sentence it displaced. Mermaid renders on every major forge and in most
editors.

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

The decisions that would otherwise surprise someone: a deliberate exception, a
limit that looks like a bug, an ordering that has to hold.

## Related

The neighbouring domains, and how this one connects to them.
