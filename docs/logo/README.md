# spoolway logo

`lockup-a-mark-left.png` — a spool of thread ahead of the name. The mark is a six-by-six pixel
drawing: flanges the full width, a barrel inset by one, and the thread as a checker running corner
to corner. `mark.py` holds it as six strings, which is the whole source — the PNGs and the board's
art both draw this one, frame A.

`mark.py` also holds a second frame, `GRID_B`, hand-drawn the same way with the barrel's thread on
the opposite phase. Nothing here uses it yet: it exists so a later piece of work can animate the
mark by swapping between the two, without either frame being derived from the other at runtime.

The background is transparent (RGBA, the ink mask becomes the alpha), so the mark sits directly on
the page rather than in a white card. It ships twice: `lockup-a-mark-left.png` is black ink for light
backgrounds, `lockup-a-mark-left-invert.png` is white ink for dark ones. The two are drawn
separately rather than inverted with a filter — `filter: invert(1)` inverts the antialiasing along
with the ink and fringes the mark in grey.

## Whole numbers only

Both halves of the lockup are pixel art — the mark on its six-by-six grid, the name in a pixel font
— and they are drawn at **the same block size**. One design pixel of the mark is one design pixel of
the type, everywhere. Nothing is ever resampled and the mark is never scaled to fit.

That rule is what makes the two read as one drawing instead of an icon standing beside some type,
and it is also what sets the mark's apparent size correctly. The wordmark's box is ten design
pixels, but seven of those are the ascender on `l` and the descenders on `p` and `y`; the letters
themselves stand five. The mark stands six, and sat on the baseline it fills the band the letters
fill, one pixel over.

Scaling the mark to "match the wordmark's height" is the mistake to avoid, and it was made twice
here before this note existed. It matches the box rather than the letters, so the mark towers over
the word, and it doubles the mark's blocks so a coarse drawing sits next to a fine one. If the mark
should be a different size, redraw it on the grid — do not scale it.

This is not a stylistic preference, or not only one. The mark was briefly drawn from geometry —
ellipses for the caps, a rasteriser to bring it down to size — and it looked correct everywhere
except the place it actually ships. On the dispatcher board at twelve pixel rows its caps came out
one pixel tall; a terminal leaves a little air between character rows, so a one-pixel horizontal
stroke reads as a gap rather than a line, and the mark arrived looking like `]_[`. A drawing made
on the grid it will be displayed on does not have that failure mode.

## Provenance

Drawn here, by hand, on a grid. No stock asset is involved and none may be: the usual clip-art
licenses forbid using their work as part of a logo or trademark at every tier, so a purchased spool
would be no more usable than a free one.

## Using it

Relative paths work on github.com once the file is committed, and since the background is
transparent, black ink vanishes on GitHub's dark theme — so pair the invert. This is what the
top-level README does:

```markdown
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo/lockup-a-mark-left-invert.png">
  <img src="docs/logo/lockup-a-mark-left.png" alt="spoolway" width="350">
</picture>
```

The displayed `width` follows the whole-numbers rule as well. The pair is 800 by 224 pixels, which
is fifty design pixels across at `SCALE = 16`, so only a multiple of fifty lands one design pixel on
a whole number of screen pixels. At `360` the browser resamples and the blocks come out uneven and
soft; `350` is seven screen pixels per design pixel and stays crisp.

Note that `crates.io` and `npmjs.com` render the README without the repo around it, so if the logo
should show up there too, the `src` needs an absolute
`https://raw.githubusercontent.com/<owner>/spoolway/main/docs/logo/…` URL.

The plan skeleton — the only HTML page spoolway writes — points at both PNGs as files beside it
rather than inlining them, so a page opened from disk still says which tool wrote it on either
ground. `init` copies them next to the skeleton and `pages.rs` compares the shipped bytes against
these files, so regenerating the logo means committing both copies together or the build fails.

## Regenerating

```sh
python3 mark.py                # the mark alone, to the terminal and mark-preview.png
python3 generate_lockups.py    # the shipped pair -> lockup-a-mark-left{,-invert}.png
python3 generate_board.py      # the dispatcher's art -> paste into src/status.rs
```

To change frame A, edit `GRID`'s six strings in `mark.py` and run the other two — `GRID_B` moves
only if the second frame needs the same change. All three need Pillow. `generate_board.py` prints
the lockup as half-block characters at five heights for each frame, then the two-frame `LOCKUP`
constant for `src/status.rs` at the height the board actually uses. Paste it; `status.rs` has a
test that runs this script and fails if the two have drifted, which is what stops the board's art
from becoming a hand transcription again.

`generate.py` still holds the grid helpers, `SCALE`, and ten discarded mark drafts from the
exploration that produced the old reel. `generate_wordmarks.py` holds the pixel font the lockup
draws the name with, and its own discarded wordmark drafts. Running either re-emits those drafts;
delete whatever you don't want back.
