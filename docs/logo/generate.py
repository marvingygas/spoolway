#!/usr/bin/env python3
"""Generate 1-bit (black & white) pixel-art logo marks for spoolway."""
import os
from PIL import Image

OUT = os.path.dirname(os.path.abspath(__file__))
SCALE = 16


def save_pair(gray, name):
    """Write black-ink and white-ink PNGs, both on a transparent background.

    `gray` is a strictly 0/255 image: 0 is ink, 255 is background. The ink mask
    becomes the alpha channel, so nothing but the drawing is opaque and the mark
    sits directly on whatever page it lands in.
    """
    alpha = gray.point(lambda v: 255 - v)
    for suffix, ink in (("", (0, 0, 0)), ("-invert", (255, 255, 255))):
        img = Image.new("RGBA", gray.size, ink + (0,))
        img.putalpha(alpha)
        img.save(f"{OUT}/{name}{suffix}.png")


class G:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.d = [[0] * w for _ in range(h)]

    def px(self, x, y, v=1):
        if 0 <= x < self.w and 0 <= y < self.h:
            self.d[y][x] = v

    def rect(self, x0, y0, x1, y1, v=1):
        for y in range(y0, y1 + 1):
            for x in range(x0, x1 + 1):
                self.px(x, y, v)

    def frame(self, x0, y0, x1, y1, t=2, v=1):
        self.rect(x0, y0, x1, y0 + t - 1, v)
        self.rect(x0, y1 - t + 1, x1, y1, v)
        self.rect(x0, y0, x0 + t - 1, y1, v)
        self.rect(x1 - t + 1, y0, x1, y1, v)

    def ring(self, cx, cy, ro, ri, v=1):
        for y in range(self.h):
            for x in range(self.w):
                d = ((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) ** 0.5
                if ri <= d <= ro:
                    self.px(x, y, v)

    def disc(self, cx, cy, r, v=1):
        self.ring(cx, cy, r, -1.0, v)

    def line(self, x0, y0, x1, y1, t=1, v=1):
        dx, dy = abs(x1 - x0), -abs(y1 - y0)
        sx = 1 if x0 < x1 else -1
        sy = 1 if y0 < y1 else -1
        err = dx + dy
        while True:
            self.rect(x0, y0, x0 + t - 1, y0 + t - 1, v)
            if x0 == x1 and y0 == y1:
                break
            e2 = 2 * err
            if e2 >= dy:
                err += dy
                x0 += sx
            if e2 <= dx:
                err += dx
                y0 += sy

    def chevron(self, x, ytop, ybot, t=2, v=1):
        ymid = (ytop + ybot) // 2
        w = (ybot - ytop) // 2
        self.line(x, ytop, x + w, ymid, t, v)
        self.line(x + w, ymid, x, ybot, t, v)

    def wind(self, x0, y0, x1, y1, step=3, inset=1):
        """Carve white winding gaps into a filled core so it reads as thread."""
        for y in range(y0 + step - 1, y1, step):
            self.rect(x0 + inset, y, x1 - inset, y, 0)

    def save(self, name):
        img = Image.new("L", (self.w, self.h), 255)
        px = img.load()
        for y in range(self.h):
            for x in range(self.w):
                px[x, y] = 0 if self.d[y][x] else 255
        big = img.resize((self.w * SCALE, self.h * SCALE), Image.NEAREST)
        save_pair(big, name)
        return big


def l01_spool_side():
    """01 - the spool itself, side on: two flanges, wound core."""
    g = G(32, 32)
    g.rect(3, 3, 28, 7)          # top flange
    g.rect(8, 8, 23, 23)         # wound core
    g.wind(8, 8, 23, 23, step=3)
    g.rect(3, 24, 28, 28)        # bottom flange
    return g, "01-spool-side"


def l02_reel_front():
    """02 - the reel head-on: rim, hub, four spokes."""
    g = G(32, 32)
    g.ring(16, 16, 14.5, 12.0)
    g.rect(15, 4, 16, 27)
    g.rect(4, 15, 27, 16)
    g.line(8, 8, 23, 23, 2)
    g.line(23, 8, 8, 23, 2)
    g.disc(16, 16, 4.5)
    g.disc(16, 16, 1.6, 0)
    return g, "02-reel-front"


def l03_spool_feed():
    """03 - spool paying thread out into the pipeline."""
    g = G(48, 32)
    g.ring(11, 16, 10.0, 7.5)
    g.rect(10, 6, 11, 25)
    g.rect(1, 15, 20, 16)
    g.disc(11, 16, 3.2)
    g.rect(21, 15, 30, 16)       # thread running right
    for i, x in enumerate((30, 36, 42)):
        g.chevron(x, 9, 23, 2)
    return g, "03-spool-feed"


def l04_queue_to_spool():
    """04 - queued tasks drawn onto the spool."""
    g = G(48, 32)
    for y in (5, 14, 23):
        g.rect(2, y, 13, y + 4)
    g.rect(16, 15, 24, 16)       # arrow shaft
    for i in range(4):           # arrow head
        g.rect(24 + i, 13 + i, 24 + i, 18 - i)
    g.rect(30, 4, 46, 8)
    g.rect(34, 9, 42, 22)
    g.wind(34, 9, 42, 22, step=3)
    g.rect(30, 23, 46, 27)
    return g, "04-queue-to-spool"


def l05_s_monogram():
    """05 - the S itself wound between two spool flanges."""
    g = G(32, 32)
    g.rect(2, 2, 29, 5)          # top flange
    g.rect(6, 7, 25, 10)         # S
    g.rect(6, 10, 9, 14)
    g.rect(6, 14, 25, 17)
    g.rect(22, 17, 25, 21)
    g.rect(6, 21, 25, 24)
    for i in range(3):           # chamfer the two curved corners
        g.rect(6, 7 + i, 8 - i, 7 + i, 0)
        g.rect(23 + i, 24 - i, 25, 24 - i, 0)
    g.rect(2, 26, 29, 29)        # bottom flange
    return g, "05-s-monogram"


def l06_reel_to_reel():
    """06 - reel to reel: work travelling from one spool to the next."""
    g = G(48, 32)
    for cx in (9, 39):
        g.ring(cx, 16, 8.5, 6.8)
        g.rect(cx - 1, 8, cx, 23)
        g.rect(cx - 8, 15, cx + 7, 16)
        g.disc(cx, 16, 2.2)
    g.rect(19, 15, 25, 16)       # thread handed on to the next reel
    for i in range(4):
        g.rect(25 + i, 12 + i, 25 + i, 19 - i)
    return g, "06-reel-to-reel"


def l07_spool_stack():
    """07 - spool over a three-stage pipeline."""
    g = G(32, 32)
    g.rect(5, 2, 26, 6)
    g.rect(10, 7, 21, 17)
    g.wind(10, 7, 21, 17, step=3)
    g.rect(5, 18, 26, 22)
    for x in (3, 12, 21):
        g.rect(x, 26, x + 7, 30)
    return g, "07-spool-stack"


def l08_terminal_reel():
    """08 - a reel spinning in a terminal pane."""
    g = G(32, 32)
    g.frame(1, 2, 30, 29, 2)
    g.rect(1, 2, 30, 8)          # title bar
    for x in (4, 8, 12):
        g.rect(x, 4, x + 1, 5, 0)
    g.line(6, 13, 11, 18, 2)     # prompt caret
    g.line(11, 18, 6, 23, 2)
    g.rect(6, 25, 13, 26)
    g.ring(21, 19, 6.5, 4.5)
    g.rect(20, 13, 21, 25)
    g.rect(15, 18, 26, 19)
    g.disc(21, 19, 2.2)
    return g, "08-terminal-reel"


def l09_thread_to_merge():
    """09 - thread unwound from the spool, landing as a merged node."""
    g = G(32, 32)
    g.ring(10, 10, 9.0, 6.5)
    g.rect(9, 3, 10, 17)
    g.rect(2, 9, 17, 10)
    g.disc(10, 10, 2.8)
    g.rect(9, 18, 10, 23)        # staircase thread
    g.rect(9, 22, 21, 23)
    g.rect(20, 22, 21, 27)
    g.disc(24, 24, 7.2)
    g.line(21, 24, 23, 26, 2, 0)  # white check
    g.line(23, 26, 27, 21, 2, 0)
    return g, "09-thread-to-merge"


def l10_hex_badge():
    """10 - hex badge: reel and hub inside a chip-like frame."""
    g = G(32, 32)
    pts = [(16, 1), (29, 9), (29, 23), (16, 30), (3, 23), (3, 9)]
    for i in range(6):
        x0, y0 = pts[i]
        x1, y1 = pts[(i + 1) % 6]
        g.line(x0, y0, x1, y1, 2)
    g.ring(16, 16, 8.5, 6.5)
    g.rect(15, 8, 16, 24)
    g.rect(8, 15, 24, 16)
    g.disc(16, 16, 3.0)
    g.disc(16, 16, 1.0, 0)
    return g, "10-hex-badge"


def main():
    os.makedirs(OUT, exist_ok=True)
    makers = [l01_spool_side, l02_reel_front, l03_spool_feed, l04_queue_to_spool,
              l05_s_monogram, l06_reel_to_reel, l07_spool_stack, l08_terminal_reel,
              l09_thread_to_merge, l10_hex_badge]
    tiles = []
    for m in makers:
        g, name = m()
        tiles.append((g.save(name), name))
        print(f"{name}: {g.w}x{g.h} px art -> {g.w*SCALE}x{g.h*SCALE}")

    # contact sheet for review
    cw, ch, cols = 800, 620, 4
    sheet = Image.new("L", (cols * cw // 2, ((len(tiles) + cols - 1) // cols) * ch // 2), 255)
    for i, (img, name) in enumerate(tiles):
        t = img.copy()
        t.thumbnail((cw // 2 - 20, ch // 2 - 20))
        x = (i % cols) * cw // 2 + (cw // 2 - t.width) // 2
        y = (i // cols) * ch // 2 + (ch // 2 - t.height) // 2
        sheet.paste(t, (x, y))
    sheet.save(f"{OUT}/preview.png")


if __name__ == "__main__":
    main()
