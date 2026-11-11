#!/usr/bin/env python3
"""给 Tervo/X11 终端截图叠加 1-based 行号，便于对照 report。"""
import sys
from PIL import Image, ImageDraw, ImageFont

src, dst = sys.argv[1], sys.argv[2]
cell_h = int(sys.argv[3]) if len(sys.argv) > 3 else 22
top = int(sys.argv[4]) if len(sys.argv) > 4 else 32
im = Image.open(src).convert('RGB')
w, h = im.size
rows = max(0, (h - top) // cell_h)
out = Image.new('RGB', (w + 90, h), (20, 20, 20))
out.paste(im, (90, 0))
d = ImageDraw.Draw(out)
try:
    font = ImageFont.truetype('/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf', 16)
except Exception:
    font = ImageFont.load_default()
for r in range(rows):
    y = top + r * cell_h
    d.text((8, y + 3), f'{r + 1:3d}', fill=(255, 180, 80) if r % 5 == 0 else (150, 150, 150), font=font)
    d.line((86, y, w + 90, y), fill=(60, 60, 60), width=1)
out.save(dst)
print(f'{dst}: {out.size[0]}x{out.size[1]} rows={rows} cell_h={cell_h} top={top}')
