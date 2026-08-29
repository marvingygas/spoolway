#!/usr/bin/env python3
"""The lockup as half-block art, for the dispatcher board.

Emits the `LOCKUP` constant that `src/status.rs` draws in its masthead. Run it
and paste; the Rust side has a test that this file and the committed constant
still agree, so the art is generated and never transcribed by hand.

Why half-blocks and not sextants: a sextant divides the cell 2x3, which makes
each sub-pixel a third taller than it is wide, so the design has to be stretched
4/3 before packing and hand-repaired afterwards where the stretch doubled a
one-pixel stroke. A half-block divides it 1x2 — square pixels, no stretch,
nothing to repair.

Why 1-bit and not greyscale: the obvious upgrade is a colour per half of the
cell, so the mark arrives antialiased. It doesn't work. Antialiasing is partial
coverage *of a known ground*, and the board does not know the terminal's
background colour — the masthead is drawn in the default foreground precisely so
it is right on a light terminal and a dark one. Paint explicit greys and the
mark is correct on whichever ground you happened to test.
"""
import os

import mark
from generate_lockups import WORD, baseline, word_grid

OUT = os.path.dirname(os.path.abspath(__file__))
ROWS = 5            # cell rows in the masthead; two design pixels each
GUTTER = 2          # design pixels between mark and word

# " ", top, bottom, both — indexed by (top << 1) | bottom.
BLOCKS = (" ", "▄", "▀", "█")


def compose(rows=ROWS, frame=mark.GRID):
    """The lockup on a grid `rows * 2` pixels tall, as a 0/1 bitmap.

    `frame` picks which of the mark's drawings sits ahead of the word — the
    board's constant needs both, so this takes the grid rather than assuming
    frame A the way the single-frame PNGs still do.
    """
    px_h = rows * 2
    grid = word_grid(WORD)
    word_h, word_w = len(grid), len(grid[0])

    # One design pixel per design pixel — the same block size the wordmark is
    # drawn at, because they are one drawing and not an icon beside some type.
    # The mark is never scaled to fit the band; if it needs to be another size
    # it gets redrawn on its own grid.
    bits = mark.bits(frame)
    mark_w, mark_h = mark.WIDTH, mark.HEIGHT

    # Stood on the word's baseline — the bottom of the letters, not the bottom
    # of the box the descenders reach. Sizing and seating the mark against the
    # letters is the whole point: they are what the eye measures it against.
    #
    # Nudged up to an even row if the baseline is odd, because a mark starting
    # halfway through a cell puts every one of its rows on a cell boundary, and
    # that is how a solid drawing turns back into half-blocks.
    top_pad = baseline() - mark_h
    if top_pad % 2:
        top_pad -= 1
    top_pad = max(0, top_pad)

    width = mark_w + GUTTER + word_w
    out = [[0] * width for _ in range(px_h)]
    for y in range(mark_h):
        for x in range(mark_w):
            out[top_pad + y][x] = bits[y][x]

    # The word sits on the baseline of the band, not centred: it is type.
    top = px_h - word_h
    for y in range(word_h):
        for x in range(word_w):
            if grid[y][x] and 0 <= top + y < px_h:
                out[top + y][mark_w + GUTTER + x] = 1
    return out


def as_blocks(bitmap):
    """Pairs of pixel rows folded into one row of half-block characters."""
    lines = []
    for cy in range(0, len(bitmap), 2):
        top, bot = bitmap[cy], bitmap[cy + 1] if cy + 1 < len(bitmap) else [0] * len(bitmap[0])
        lines.append("".join(BLOCKS[(t << 1) | b] for t, b in zip(top, bot)))
    return [line.rstrip() or " " for line in lines]


def rust_const(frames):
    """The `LOCKUP` constant, one array of lines per frame.

    Two frames rather than one: the board draws frame A only, but the
    constant carries both so the task that picks between them has the second
    one already generated instead of pasted in by hand.
    """
    frame_bodies = []
    for lines in frames:
        body = "\n".join(f'        "{line}",' for line in lines)
        frame_bodies.append(f"    [\n{body}\n    ],")
    body = "\n".join(frame_bodies)
    rows = len(frames[0])
    return f"const LOCKUP: [[&str; {rows}]; {len(frames)}] = [\n{body}\n];"


if __name__ == "__main__":
    for rows in (4, 5, 6, 7, 8):
        lines = as_blocks(compose(rows))
        print(f"--- {rows} rows ({rows * 2} pixels tall) ---")
        print("\n".join(lines))
    print()
    print(rust_const([as_blocks(compose(frame=mark.GRID)), as_blocks(compose(frame=mark.GRID_B))]))
