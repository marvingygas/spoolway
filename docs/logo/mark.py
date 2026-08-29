#!/usr/bin/env python3
"""The mark: a spool, drawn by hand on a six-by-six grid.

Six by six, exactly as drawn. It is not resized to fit anything, and that is the
rule: **one design pixel of the mark is one design pixel of the wordmark**,
everywhere it is drawn. The two are the same pixel art at the same block size,
so they read as one drawing rather than as an icon placed next to some type.

That rule is what sets the mark's apparent size, and it is the right one. The
mark stands six pixels; the letters stand five, with the rest of the wordmark's
ten-pixel box taken up by the ascender on `l` and the descenders on `p` and `y`.
Sat on the baseline, the mark fills the band the letters fill, one pixel over.

Scaling it up to "match the wordmark's height" is the mistake that was made
here twice: it matches the *box*, not the letters, and it doubles the block size
so the mark reads as a coarser drawing sitting beside a finer one. If the mark
needs to be a different size, redraw it on this grid — do not scale it.


This is the logo, and it is a bitmap rather than geometry on purpose. The
previous mark was built out of ellipses and rasterised down, which looked right
in a browser and fell apart in a terminal: at twelve pixel rows its caps came
out one pixel tall, and a terminal puts a little air between character rows, so
a one-pixel horizontal stroke reads as a gap rather than as a line. The mark
arrived on the board as `]_[`.

Drawn straight on the grid instead, every stroke is at least a pixel that
survives — flanges the full width, a barrel inset by one, and the thread as a
checker running corner to corner. What you see here is what every render shows,
because there is no resampling anywhere: the mark only ever scales by whole
numbers.

    GRID, GRID_B        -> the rows, as strings
    bits(grid)           -> the rows, as 0/1 lists
    image(scale, grid)  -> greyscale, `scale` pixels per design pixel
"""
import os

from PIL import Image

GRID = (
    "######",
    ".#.##.",
    ".##.#.",
    ".#.##.",
    ".##.#.",
    "######",
)

# The same spool, thread on the opposite phase. Flanges, outer columns and
# silhouette are untouched — only the barrel's two inner columns (index 2 and
# 3) flip, row by row, so a checker running one way in frame A runs the other
# way here. Drawn by hand on the grid, like `GRID`, and not derived from it:
# deriving it would make the two the same drawing wearing a runtime transform
# rather than two frames of the same hand-drawn mark.
GRID_B = (
    "######",
    ".##.#.",
    ".#.##.",
    ".##.#.",
    ".#.##.",
    "######",
)

WIDTH = len(GRID[0])
HEIGHT = len(GRID)


def bits(grid=GRID):
    """`grid`'s rows as 0 and 1. Frame A unless another grid is passed."""
    return [[1 if c == "#" else 0 for c in row] for row in grid]


def image(scale=1, grid=GRID):
    """`grid` at `scale` pixels per design pixel. 0 is ink, 255 is ground.

    Nearest-neighbour and integer-only. Any other resampling puts grey on the
    edges of a drawing whose whole character is that it has none.
    """
    img = Image.new("L", (WIDTH, HEIGHT), 255)
    px = img.load()
    for y, row in enumerate(grid):
        for x, c in enumerate(row):
            if c == "#":
                px[x, y] = 0
    if scale == 1:
        return img
    return img.resize((WIDTH * scale, HEIGHT * scale), Image.NEAREST)


if __name__ == "__main__":
    out = os.path.dirname(os.path.abspath(__file__))
    image(64).save(f"{out}/mark-preview.png")
    for row in GRID:
        print(row.replace("#", "██").replace(".", "  "))
