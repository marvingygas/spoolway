#!/usr/bin/env python3
"""Generate 1-bit (black & white) "spoolway" wordmarks: pixel-drawn and typeset."""
import os
from PIL import Image, ImageDraw, ImageFont, ImageOps

from generate import G, SCALE, save_pair

OUT = os.path.dirname(os.path.abspath(__file__))
WORD = "spoolway"

# --- pixel font ------------------------------------------------------------
# 5-row x-height, drawn on a common baseline; `top` shifts a glyph up (ascender)
# and rows past the fifth hang below the baseline (descender).
GLYPHS = {
    "s": (0, [".###",
              "#...",
              ".##.",
              "...#",
              "###."]),
    "p": (0, ["###.",
              "#..#",
              "#..#",
              "###.",
              "#...",
              "#...",
              "#..."]),
    "o": (0, [".##.",
              "#..#",
              "#..#",
              "#..#",
              ".##."]),
    "l": (-3, ["#.",
               "#.",
               "#.",
               "#.",
               "#.",
               "#.",
               "#.",
               "##"]),
    "w": (0, ["#...#",
              "#...#",
              "#.#.#",
              "##.##",
              "#...#"]),
    "a": (0, [".##.",
              "...#",
              ".###",
              "#..#",
              ".###"]),
    "y": (0, ["#..#",
              "#..#",
              "#..#",
              ".###",
              "...#",
              "...#",
              "###."]),
    # a reel seen head-on, drawn to the same metrics, standing in for an "o"
    "@": (0, [".###.",
              "#...#",
              "#.#.#",
              "#...#",
              ".###."]),
}


def pixel_wordmark(text, spacing=1, pad=2, plate=False):
    glyphs = [GLYPHS[c] for c in text]
    top = min(g[0] for g in glyphs)
    bottom = max(g[0] + len(g[1]) for g in glyphs)
    w = sum(len(g[1][0]) for g in glyphs) + spacing * (len(glyphs) - 1) + 2 * pad
    h = (bottom - top) + 2 * pad
    g = G(w, h)
    ink, paper = 1, 0
    if plate:
        g.rect(0, 0, w - 1, h - 1)
        ink, paper = 0, 1
    x = pad
    for gtop, rows in glyphs:
        for j, row in enumerate(rows):
            for i, ch in enumerate(row):
                if ch == "#":
                    g.px(x + i, pad + (gtop - top) + j, ink)
        x += len(rows[0]) + spacing
    return g


# --- typeset ---------------------------------------------------------------
FONTS = "/usr/share/fonts/truetype"


def save_bw(img, name, pad=20, scale=1):
    """Threshold to pure black/white, trim, pad, and write both polarities."""
    img = img.point(lambda v: 0 if v < 128 else 255)
    box = ImageOps.invert(img).getbbox()
    img = img.crop(box)
    out = Image.new("L", (img.width + 2 * pad, img.height + 2 * pad), 255)
    out.paste(img, (pad, pad))
    if scale != 1:
        out = out.resize((out.width * scale, out.height * scale), Image.NEAREST)
    save_pair(out, name)
    return out


def typeset(name, font_path, text, size=180, tracking=0, pad=20, scale=1):
    font = ImageFont.truetype(font_path, size)
    widths = [font.getlength(c) for c in text]
    total = int(sum(widths) + tracking * (len(text) - 1))
    img = Image.new("L", (total + 4 * size, 3 * size), 255)
    d = ImageDraw.Draw(img)
    x = float(size)
    for c, w in zip(text, widths):
        d.text((x, size), c, font=font, fill=0)
        x += w + tracking
    return save_bw(img, name, pad, scale)


def main():
    made = []

    g = pixel_wordmark(WORD, spacing=1)
    made.append((g.save("wordmark-01-pixel"), "01 pixel, tight"))

    g = pixel_wordmark(WORD, spacing=3)
    made.append((g.save("wordmark-02-pixel-spaced"), "02 pixel, spaced"))

    g = pixel_wordmark("sp@@lway", spacing=1)
    made.append((g.save("wordmark-03-pixel-spool"), "03 pixel, spools for o's"))

    g = pixel_wordmark(WORD, spacing=2, pad=3, plate=True)
    made.append((g.save("wordmark-04-pixel-plate"), "04 pixel, knocked out of a plate"))

    made.append((typeset("wordmark-05-unifont", "/usr/share/fonts/opentype/unifont/unifont.otf",
                         WORD, size=16, tracking=0, pad=2, scale=SCALE), "05 unifont bitmap"))

    made.append((typeset("wordmark-06-sans-bold", f"{FONTS}/dejavu/DejaVuSans-Bold.ttf",
                         WORD, tracking=-6), "06 DejaVu Sans Bold"))

    made.append((typeset("wordmark-07-mono", f"{FONTS}/dejavu/DejaVuSansMono-Bold.ttf",
                         WORD), "07 DejaVu Sans Mono Bold"))

    made.append((typeset("wordmark-08-ubuntu", f"{FONTS}/ubuntu/Ubuntu-B.ttf",
                         WORD, tracking=6), "08 Ubuntu Bold, tracked"))

    made.append((typeset("wordmark-09-caps-tracked", f"{FONTS}/liberation/LiberationSans-Bold.ttf",
                         WORD.upper(), size=140, tracking=28), "09 Liberation Sans Bold, caps"))

    made.append((typeset("wordmark-10-serif", f"{FONTS}/dejavu/DejaVuSerif-Bold.ttf",
                         WORD), "10 DejaVu Serif Bold"))

    # stacked preview sheet
    width = 1100
    rows = []
    for img, _ in made:
        r = img.copy()
        r.thumbnail((width - 60, 220))
        rows.append(r)
    sheet = Image.new("L", (width, sum(r.height + 30 for r in rows) + 30), 255)
    y = 30
    for r in rows:
        sheet.paste(r, ((width - r.width) // 2, y))
        y += r.height + 30
    sheet.save(f"{OUT}/wordmarks-preview.png")
    for _, label in made:
        print(label)


if __name__ == "__main__":
    main()
