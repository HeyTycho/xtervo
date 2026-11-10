#!/usr/bin/env python3
"""Pixel checks for the dsh-tui image-preview e2e.

Subcommands (all paths are PNG screenshots captured with xwd + xwd2png.py):

  magenta <png>
      Print the number of saturated-magenta pixels. The fixture image has a
      solid #ff00ff border, so this is a cheap "is the preview image actually
      on screen" probe.

  diff <a.png> <b.png> [--threshold N]
      Print the number of pixels whose RGB channels differ by more than N
      (default 24) between two screenshots, plus the bounding box of the
      changed region. Used for the "no residue after close" assertion.

  dark-band <png> [--y0 N] [--y1 N]
      Find the widest contiguous band of rows (inside the transcript area)
      that contains many dark pixels, and print the centre of that band's
      dark-pixel bounding box as local window coordinates: ``cx cy width
      height dark_pixels``. dsh-tui renders transcript image thumbnails as
      dark blocks on the light theme, so this locates the thumbnail to click
      without hard-coding a font metric.

These are test helpers only; no product code depends on them.
"""

from __future__ import annotations

import argparse
import sys

from PIL import Image


def load(path: str) -> Image.Image:
    return Image.open(path).convert("RGB")


def is_magenta(r: int, g: int, b: int) -> bool:
    return r > 240 and g < 24 and b > 240


def cmd_magenta(args: argparse.Namespace) -> int:
    im = load(args.png)
    count = sum(1 for r, g, b in im.getdata() if is_magenta(r, g, b))
    print(count)
    return 0


def cmd_diff(args: argparse.Namespace) -> int:
    a, b = load(args.a), load(args.b)
    if a.size != b.size:
        print(f"size-mismatch {a.size} {b.size}")
        return 2
    changed = 0
    x0 = y0 = 10**9
    x1 = y1 = -1
    for y in range(a.height):
        row_a = [a.getpixel((x, y)) for x in range(a.width)]
        row_b = [b.getpixel((x, y)) for x in range(b.width)]
        for x, (pa, pb) in enumerate(zip(row_a, row_b)):
            if (
                abs(pa[0] - pb[0]) > args.threshold
                or abs(pa[1] - pb[1]) > args.threshold
                or abs(pa[2] - pb[2]) > args.threshold
            ):
                changed += 1
                x0, x1 = min(x0, x), max(x1, x)
                y0, y1 = min(y0, y), max(y1, y)
    if changed == 0:
        print("0 0 0 0 0")
    else:
        print(f"{changed} {x0} {y0} {x1} {y1}")
    return 0


def cmd_dark_band(args: argparse.Namespace) -> int:
    im = load(args.png)
    y0 = max(0, args.y0)
    y1 = min(im.height, args.y1 if args.y1 > 0 else im.height)

    # Count dark pixels per row; a transcript image block is a wide run of
    # dark pixels, ordinary text never produces hundreds per row.
    rows: list[tuple[int, int]] = []  # (row, dark count)
    for y in range(y0, y1):
        dark = 0
        for x in range(0, im.width, 2):  # half sampling is plenty
            r, g, b = im.getpixel((x, y))
            if r + g + b < 180:
                dark += 1
        rows.append((y, dark * 2))

    bands: list[tuple[int, int]] = []
    start = None
    for y, dark in rows:
        if dark >= args.min_dark:
            if start is None:
                start = y
        elif start is not None:
            bands.append((start, y - 1))
            start = None
    if start is not None:
        bands.append((start, y1 - 1))
    if not bands:
        print("none")
        return 3

    # Prefer the band with the most dark pixels (not merely the tallest).
    best = max(
        bands,
        key=lambda band: sum(
            d for yy, d in rows if band[0] <= yy <= band[1]
        ),
    )
    xs: list[int] = []
    for y in range(best[0], best[1] + 1):
        for x in range(im.width):
            r, g, b = im.getpixel((x, y))
            if r + g + b < 180:
                xs.append(x)
    if not xs:
        print("none")
        return 3
    x0, x1 = min(xs), max(xs)
    total = sum(d for yy, d in rows if best[0] <= yy <= best[1])
    print(
        f"{(x0 + x1) // 2} {(best[0] + best[1]) // 2} "
        f"{x1 - x0 + 1} {best[1] - best[0] + 1} {total}"
    )
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("magenta")
    p.add_argument("png")
    p.set_defaults(func=cmd_magenta)

    p = sub.add_parser("diff")
    p.add_argument("a")
    p.add_argument("b")
    p.add_argument("--threshold", type=int, default=24)
    p.set_defaults(func=cmd_diff)

    p = sub.add_parser("dark-band")
    p.add_argument("png")
    p.add_argument("--y0", type=int, default=0)
    p.add_argument("--y1", type=int, default=0)
    p.add_argument("--min-dark", type=int, default=140)
    p.set_defaults(func=cmd_dark_band)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
