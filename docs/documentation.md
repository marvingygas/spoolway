---
domain: documentation
covers: ["src/skeleton.rs", "src/task_template.rs", "assets/tasks/**", "assets/prompts/archivist/**"]
---

# Documentation and templates

spoolway writes three kinds of page into your project: domain documents, task files, and the
prompt files that produce them. Every one of them is a skeleton you own, not a shape a prompt
carries. A plan is a fourth kind of page, but not spoolway's: `spoolway-plan` writes it from a
skeleton under its own skill directory, and the binary never opens the result — see
[Planning](planning.md#the-plan-file).

## Documentation is per domain, not per plan

The documentation directory holds **one document per domain of the system**, each declaring
which paths it covers, as YAML frontmatter:

```yaml
---
domain: api
covers: ["src/api/**", "src/routes/**"]
---
```

spoolway keeps no notion of documentation in the binary at all: nothing loads a document,
reads this header, or checks it against anything. The header is a convention the archivist
prompt owns start to finish — it is what lets the archivist route a later task's changed
paths back to the right document, by reading the directory and matching `covers` itself.
Anything else you want in the header — a title, a nav weight — is your project's business;
the archivist reads the two keys above and leaves the rest alone.

### How a document gets updated

Both shipped pipelines give every task a `document` step. Its lane runs the archivist
prompt, and the prompt is what resolves the task's `touches` against every document's
`covers` — by reading `docs/` itself, not from anything spoolway hands it. The repository's own
front pages, `README.md` and `DOCS.md`, are that prompt's too, and no other step may touch
them. See `assets/prompts/archivist/PROMPT.md`.

The scope is one task's diff, not a plan's. That is what puts the documentation in the same
pull request as the behaviour it describes, rather than in a separate change at the end.

A glob that no longer matches where the code lives is how a document starts rotting, which is
why `covers` is worth keeping honest.

### Documents describe the system as it is

They are not changelogs. Nobody reading them wants to know what a plan proposed, what
deviated, or what changed last week — git already holds that. They want an accurate
description of how the thing works today, written as if the code had always been this way.

That is the archivist prompt's whole brief, and it is the reason the `document` step exists
at all. The shape a document takes is a skeleton in the archivist's own `assets/` directory, and
the documents themselves sit in `docs/` at the project root, each carrying its header where
anyone can read it.

## What a page looks like is a file, not a prompt

Documents live in `docs/`, tracked in git alongside the code they describe — a document is
read for as long as the code lives. There is no setting for the location or the shape: the
directory is fixed, and what gets written there — Markdown carrying a `domain`/`covers`
header as YAML frontmatter, the shipped default, or anything else a project's own archivist
prompt chooses to write instead — is entirely that prompt's call.

Their skeletons are the archivist's belongings, written once by setup into the prompt's own
`assets/` directory:

```
.spoolway/prompts/archivist/
  PROMPT.md
  assets/
    document.md        one per domain, and the only one carrying a routable header
    landing-page.md    one per project — the docs directory's front page
```

Two files rather than one because "one per domain" and "one per project" are different
pages with different rules, and a single skeleton serving both would state neither. Adding a
third is dropping a file in beside them and naming it in the prompt.

**This is what makes restyling your documentation an edit to a file rather than an argument
with a prompt.** The archivist reads its own skeletons instead of carrying a shape in its
prose — which is what lets one shipped prompt serve every project, and what stops a page
being redesigned from scratch on every run. A plan's shape is the same idea one skill over:
`spoolway-plan` fills a skeleton under its own `assets/`, not a prompt's description of one —
see [The plan file](planning.md#the-plan-file).

### Nothing is enforced

spoolway has no opinion on a document's shape and nothing in the binary checks one: a document
carrying no readable `domain`/`covers` header just does not route future changes to itself,
and nothing says so out loud any more — reading the directory and noticing is the archivist's
job, same as everything else about routing.

## Task skeletons

The body of a task file comes from a skeleton the project owns outright, under
`.spoolway/templates/tasks/`.

**One skeleton per pipeline, selected by filename.** A task queued on a `bugfix` pipeline is
written from `bugfix.md` if the project wrote one, falling back to `default.md` — the same
convention as a step's prompt, which is a bare name with a default. Giving a pipeline a
shape of its own costs no configuration; a pipeline that wants another pipeline's skeleton
says so with its own `task_template:` key.

spoolway does not read a task's body at all: no heading in it is required and none is
validated. The skeleton exists for whoever fills it in and whoever reads it afterwards —
read it straight from `.spoolway/templates/tasks/`.

## Keeping the skeletons current

A **prompt**, its **assets** (the document skeletons), and a **task skeleton** are none of
them touched by `spoolway update`. None has anything generated in it: how a lane finishes
comes from the system prompt the dispatcher composes the prompt into, and a task's
frontmatter is serialised from a struct in the binary on every save. Take a shipped one back
deliberately with `spoolway update --replace <path>`.

The machinery `update` uses to bring a project's own copy of a shipped skeleton forward while
leaving a restyled one alone still exists — it is what a document skeleton or a task skeleton
would use if either ever grew a machine-read block of its own — but nothing feeds it today,
document and task skeletons being untouched and the plan skeleton belonging to the skill
rather than the project.

See [Installation and setup](installation.md#keeping-a-project-current).

## The whole idea, in one line

Every artefact spoolway produces has a machine-readable part it owns and a human-readable
part you own, and the boundary between them is marked in the file itself. That is what makes
"make our docs look like ours" a five-minute edit that survives every upgrade.
