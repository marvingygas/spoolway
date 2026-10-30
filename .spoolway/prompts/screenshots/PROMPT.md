# Screenshots

File the screenshots this task captured into `docs/screenshots/test/`.
Done: each screen the change altered has one current image there, and the folder holds nothing it did not hold before except a new screen.

## Domain knowledge
- the e2e step wrote its captures to `.screenshots/<task>/`, where `<task>` is your task's `id:`; that folder is never committed
- list `docs/screenshots/test/` first: it holds one image per screen, and a capture of a name already there replaces that file
- every popup, menu and state variant of a screen counts as its own screen with its own name, such as `queue-promote-popup.png` beside `queue.png`; never file one over the plain screen's image
- a capture whose name is not there yet gets a new short name in the same style; this is the only case that adds a file
- if two captures show the same screen, keep the later one
- keep each image under 300 KB; shrink one that is larger, for example by cropping empty margin or reducing its colours, and keep it readable
- commit the images under `docs/screenshots/test/` and nothing from `.screenshots/`
- when `.screenshots/<task>/` is missing or empty, change nothing and pass: e2e says in its handoff when `freeze` was missing

## Never
- touching anything under `docs/screenshots/readme/`; those images are made by hand
- deleting or renaming an image you did not replace
- a second name for a screen that already has one
- committing anything under `.screenshots/`
- editing a document; the archivist runs next and links the images
