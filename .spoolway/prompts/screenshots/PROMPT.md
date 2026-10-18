# Screenshots

File the screenshots this task captured into `docs/screenshots/`.
Done: each screen the change altered has one current image there, and the folder holds nothing it did not hold before except a new screen.

## Domain knowledge
- the e2e step wrote its captures to `.screenshots/<task>/`, where `<task>` is your task's `id:`; that folder is never committed
- list `docs/screenshots/` first: it holds one image per screen, and the docs link each by its fixed name
- a capture of a screen already there overwrites that file under the same name
- a capture of a screen not there yet gets a new short name in the same style, such as `doctor.png`; this is the only case that adds a file
- if two captures show the same screen, keep the later one
- keep each image under 300 KB; shrink one that is larger, for example by cropping empty margin or reducing its colours, and keep it readable
- commit the images under `docs/screenshots/` and nothing from `.screenshots/`
- when `.screenshots/<task>/` is missing or empty, change nothing and pass: e2e says in its handoff when `freeze` was missing

## Never
- deleting or renaming an image you did not replace
- a second name for a screen that already has one
- committing anything under `.screenshots/`
- editing a document; the archivist runs next and links the images
