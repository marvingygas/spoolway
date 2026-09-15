---
domain: documentation
covers: ["src/skeleton.rs", "src/task_template.rs", "assets/tasks/**", "assets/prompts/archivist/**"]
---

# Documentation and templates

spoolway writes two kinds of page into your project: domain documents and task files. Both
come from skeleton files you own. The plan page is written by the `spoolway-plan` skill from
its own skeleton. See [Planning](planning.md#the-plan-file).

## Documentation is per domain, not per plan

`docs/` holds one document per domain of the system. Each document declares the paths it
covers in YAML frontmatter:

```yaml
---
domain: api
covers: ["src/api/**", "src/routes/**"]
---
```

The binary never reads a document or this header. The archivist prompt reads `docs/`, matches a
task's `touches` against each document's `covers`, and updates the documents that match. Other
keys in the header are ignored.

### How a document gets updated

Both shipped pipelines give every task a `document` step. Its lane runs the archivist prompt
on that one task's diff, before the handover. The documentation lands in the same pull
request as the code. The prompt also keeps `README.md` and `DOCS.md` correct. See
`assets/prompts/archivist/PROMPT.md`.

### Documents describe the system as it is

A document is not a changelog. It describes how the system works today, as if the code had
always been this way. Git holds the history.

## What a page looks like is a file, not a prompt

The archivist writes documents from skeletons in its own `assets/` directory:

```
.spoolway/prompts/archivist/
  PROMPT.md
  assets/
    document.md        one per domain, carries the domain/covers header
    landing-page.md    one per project, the docs front page
```

Edit these files to change how your documentation looks. To add a kind of page, add a file
beside them and name it in the prompt.

### Nothing is enforced

The binary checks nothing about a document's shape. A document without a readable
`domain`/`covers` header receives no future updates.

## Task skeletons

The body of a task file comes from `.spoolway/templates/tasks/`. There is one skeleton per
pipeline, selected by filename: a task on the `bugfix` pipeline is written from `bugfix.md`,
or from `default.md` when there is no `bugfix.md`. A pipeline can name another skeleton with
its `task_template:` key.

spoolway never reads a task's body. No heading in it is required.

## Keeping the skeletons current

`spoolway update` never touches a prompt, its assets or a task skeleton. To take a shipped
version back, run:

```
spoolway update --replace <path>
```

See [Installation and setup](installation.md#keeping-a-project-current).
