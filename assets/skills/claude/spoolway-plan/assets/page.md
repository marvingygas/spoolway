# Page mechanics

The markup mechanics for a plan page: how to get the skeleton onto disk, what each `[[slot]]`
takes, how a record's and a mockup's markup is built, and the proof block. `SKILL.md`'s step 4
opens this at the point it fills a page; step 5 opens it again for a revision. Nothing here is
judgement — what a decision says, how a mockup is drawn, how a task is sized — that is
`SKILL.md`'s, argued once and not repeated here.

**The page carries no tasks.** It ends at the Mockup, plus Open questions when something was
deferred. A breakdown is a set of task documents written beside the page, into the pending
directory — see `SKILL.md`'s step 7 — never markup appended here.

## Copy the skeleton

`cp assets/template.html` to the plan file — by default
`~/.spoolway/<project>/plans/<YYYY-MM-DD>-<slug>.html`, where `<project>` is the basename of the
repo root, `<YYYY-MM-DD>` is the day the plan is cut, and `<slug>` folds the goal to lowercase,
digits and hyphens.

**Never write a plan into the checkout.** Nothing resolves this path — it is a convention this
skill holds, so it holds only as long as you follow it. A page under the repo's own
`.spoolway/` is untracked, sits in everybody's `git status`, and rides along in a clone that was
never meant to carry it. A person who wants it somewhere else says so, and you copy there
instead; the repo is not that somewhere.

**Never Write a plan page whole**, to create it or to fill it. The two lockups are about 3,900
characters of base64 between them, and a whole-file Write re-emits every one of those characters
through a model. `cp` makes them identical by construction; a Write makes them identical only as
long as nothing slips, and eventually something does — a run of `A`s swallows four real
characters, the zlib stream stops inflating, and the page paints the word "spoolway" where the
logo goes.

## The palette: what is re-pitchable

**One thing in the `<style>` block is meant to be re-pitched: the ten values under "SUBJECT" in
`:root`** — `--paper`, `--surface`, `--ink`, `--rule`, `--accent`, each in a light and a dark
value. A plan is read once, by one person, about one subject, so take its accent and neutrals
from that subject's own world — change them there and nowhere else; a hex typed anywhere else on
the page is a colour the theme toggle throws away.

**Everything else in the `<style>` block is already decided and is not yours to edit** — the
rail, the card and figure styles, the whole of it. Restyling a plan is a wrong idea, not a
missing setting: a plan is read once and archived, and a page arguing in a house style of its
own is a page a reader has to re-learn. The two lockups are inlined as data URIs and are not
slots either — leave the `<img>` tags exactly as they are.

**Optional: one hue per part**, `--l-<part>: #......;`, when a plan has a natural set of parts
and the colour should mean the same thing in every drawing it appears in. Declare each one in the
same `:root` block as the ten, named for the part. If a token needs lifting on the dark ground
the way the ten are, add its own guard beside the ten's two, since the rest of the stylesheet has
never heard of it. Skip this entirely and every figure uses the accent.

## Fill from the slot list, never from a read

Every fillable spot is marked `[[like this]]`. Find them all, with their line numbers, in one
command — never open the page to read it:

```
grep -n '\[\[' <path>
```

Edit each line the grep names, in place, with **Edit**. The stylesheet and the two lockups carry
no marker and are never touched.

## What each slot takes

- `<title>` and `.slug` — the plan's own slug, the same string twice.
- The rail's nested lists — one `<li>` per decision and mockup step, added or removed to
  match what the page actually has, each `href`/`id` pair using the same slug.
- `<h1>` — the branch's own name for this, not a title you invent.
- `.tagline` — what is being built, in five words.
- `.where` — the absolute path this page is written to; a fact you already have, never a
  question for the person.
- `.standfirst` — one sentence a person can approve or reject.
- Context's paragraph — two or three sentences: what is true today, and the pressure on it.
- Context's figure — the one view of the system as it is, inline SVG with an `aria-label`
  summarising it; delete the whole `<figure>` only when the context genuinely has no shape.
- `.refs` lists (context and each decision) — a `<code>path</code>` and what it already explains;
  delete the list where there is nothing to point at.
- Each decision's `<h3>` — the decision stated as a decision, not a component.
- `.forces` — one or two sentences on what makes this a choice at all.
- The decision's figure — drawn or mocked, per "The record's figure" below.
- The paragraph after the figure — what it does not show: the name, the default, the thing a
  reader would otherwise get wrong.
- `.cost` — one closing sentence naming what the decision costs. Never empty; a record with
  nothing here was not a decision.
- Each mockup step's `<h3>` — two or three words naming the moment — and its figure. Nothing
  else: no caption, no note under a panel.

## The record's figure

A figure argues the decision; the prose around it only says what to look at.

- **A drawing** — `<div class="frame">` around an inline `<svg>` — for a decision about how parts
  connect. Use the `.node` / `.edge` / `.mark` classes so it follows the theme; add `.lit` to the
  frame only when the drawing's own colours carry meaning.
- **A mock** — `<div class="mocks">` — for a decision that lands in a file, a command's output,
  or a screen. `.mock.was` holds the artifact as it is today, marked with `<span class="gone">`;
  the next `.mock` holds it as proposed, marked with `<span class="new">`, and a `<span
  class="tag">proposed</span>` in its bar. Drop the `was` panel only for something that does not
  exist yet. A command's output takes the three-dot bar — `<span class="dots">` — with the
  command itself as the bar's text.
- **Never two figures, and never two panels of one figure, side by side.** Every panel and every
  drawing takes the column's full width, one per row; half a column clips the line that carries
  the change. `.mocks` already stacks its panels — do not fight it with a grid.
- A mockup starts from the thing's own captured output — a real screenshot, a real command's real
  output, a rendered page — never redrawn from memory. Something that does not run yet has no
  output to capture; its bar names the bound the panel was drawn to hold instead, in a `<span
  class="tag">`, and the task that builds it carries proving that bound as a criterion. Writing or
  running anything, scratch scripts included, to manufacture a panel's contents is not this
  skill's to do — see `SKILL.md`'s own rule on this before you reach for a terminal.
- Source is welcome as supporting material under a figure that has already said what the code is
  for — a `<pre>` inside the same `.mock`, or beside it — never as the first thing in a record.

## The proof block

Run before you say a word, after every fill and every revision:

```
grep -c '\[\[' <path>                          # must be 0
diff <(grep -o 'iVBORw0KGgo[A-Za-z0-9+/=]*' <path>) \
     <(grep -o 'iVBORw0KGgo[A-Za-z0-9+/=]*' <skeleton>)   # must print nothing
```

`<skeleton>` is `assets/template.html` beside this file — the same one the `cp` came from. A slot
left behind reaches the reader as gibberish; a non-empty diff means a lockup did not survive the
copy and the rail will paint the word "spoolway" where the logo goes. Fix whichever of these
fires and re-run until the block is clean — never mention the loop to the human.

## Revising a plan

Edits to the sections that change, never a Write of the whole file: the lockups on a page that
already exists are correct, and re-emitting them is how they stop being. Re-run the proof block
above when done.

A page whose tasks have already been cut is still just a page — there is nothing on it to freeze
or to stamp. The documents are what carry the breakdown, and revising the page above them does
not revise them. Say so to the human: whatever is still pending, or already in the queue,
describes a shape this revision has moved past, and re-cutting is step 7's decision, not a side
effect of an edit here.
