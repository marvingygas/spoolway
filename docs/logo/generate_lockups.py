#!/usr/bin/env python3
"""The lockup: the spool mark ahead of the "spoolway" wordmark.

One rasteriser, because both halves are pixel art. The wordmark is a pixel font
and the mark is a six-by-six drawing, and both are upscaled by whole numbers
with nearest-neighbour, so no stroke in the finished image is ever a fraction of
a design pixel wide. That is the identity: it should look drawn on a grid,
because it was.
"""
import os

from PIL import Image

import mark
from generate import SCALE, save_pair
from generate_wordmarks import GLYPHS

OUT = os.path.dirname(os.path.abspath(__file__))
WORD = "spoolway"
GUTTER = 2          # design pixels between the mark and the word
MARGIN = 2          # design pixels of air around the whole lockup


def word_grid(text, spacing=1):
    """The wordmark as a 0/1 grid, cropped tight to its ink."""
    width = sum(len(GLYPHS[c][1][0]) for c in text) + (len(text) - 1) * spacing
    tops = [GLYPHS[c][0] for c in text]
    depth = max(GLYPHS[c][0] + len(GLYPHS[c][1]) for c in text)
    height = depth - min(tops)
    grid = [[0] * width for _ in range(height)]

    x = 0
    for c in text:
        gtop, rows = GLYPHS[c]
        for j, row in enumerate(rows):
            for i, ch in enumerate(row):
                if ch == "#":
                    grid[gtop - min(tops) + j][x + i] = 1
        x += len(rows[0]) + spacing
    return grid


def baseline(text=WORD):
    """The row the letters stand on, in design pixels from the top of the box.

    Everything about sizing the mark is measured from here rather than from the
    box, because the box is mostly the ascender on the `l` and the descenders on
    `p` and `y` — seven of its ten rows carry no letter body at all.
    """
    tops = [GLYPHS[c][0] for c in text]
    return -min(tops) + len(GLYPHS["o"][1])


def grid_image(grid, scale):
    """A 0/1 grid as a greyscale image, upscaled without resampling."""
    h, w = len(grid), len(grid[0])
    img = Image.new("L", (w, h), 255)
    px = img.load()
    for y in range(h):
        for x in range(w):
            px[x, y] = 0 if grid[y][x] else 255
    return img.resize((w * scale, h * scale), Image.NEAREST)


def a_mark_left(scale=SCALE):
    """Mark ahead of the name — the conventional horizontal lockup."""
    grid = word_grid(WORD)
    word = grid_image(grid, scale)

    # Drawn at the wordmark's own block size: one design pixel of the mark is
    # one design pixel of the type, so both are the same pixel art rather than
    # an icon standing next to some letters. The mark is never scaled to match a
    # height — that matches the box rather than the letters, and it coarsens the
    # mark's blocks against the word's.
    glyph = mark.image(scale)

    # Stood on the word's baseline, so the two share the line they sit on.
    m = MARGIN * scale
    foot = baseline() * scale
    top = max(0, foot - glyph.height)
    height = max(glyph.height + top, word.height)
    canvas = Image.new("L", (m * 2 + glyph.width + GUTTER * scale + word.width, m * 2 + height), 255)
    canvas.paste(glyph, (m, m + top))
    canvas.paste(word, (m + glyph.width + GUTTER * scale, m))
    return canvas, "lockup-a-mark-left"


def main():
    img, name = a_mark_left()
    save_pair(img, name)
    print(f"{name}: {img.width}x{img.height}")


if __name__ == "__main__":
    main()
