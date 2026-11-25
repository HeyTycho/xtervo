#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""生成 xtervo 字标与 lockup（图标 + 字标）。

设计：JetBrains Mono Bold、紧字距、尾部琥珀光标块。
需要 Pillow（开发机生成资产用，不是运行时依赖）。

输出：
    assets/wordmark-dark.png   透明底，浅色字（深色模式）
    assets/wordmark-light.png  透明底，深色字（浅色模式）
    assets/lockup-dark.png     图标 + 字标，深色模式
    assets/lockup-light.png    图标 + 字标，浅色模式
"""
import pathlib
import sys

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

DARK_TEXT = (0xF4, 0xF6, 0xF8)
LIGHT_TEXT = (0x10, 0x13, 0x17)
AMBER = (0xE8, 0xA3, 0x3D)

FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
]

TEXT = "xtervo"
FONT_SIZE = 240
TRACKING_EM = -0.028
PAD = 40


def load_font() -> ImageFont.FreeTypeFont:
    for path in FONT_CANDIDATES:
        if pathlib.Path(path).is_file():
            return ImageFont.truetype(path, FONT_SIZE)
    print("未找到 JetBrains Mono / DejaVu Sans Mono；请安装 fonts-jetbrains-mono", file=sys.stderr)
    return ImageFont.load_default()


def render_wordmark(color, cursor: bool) -> Image.Image:
    font = load_font()
    advances = [font.getlength(ch) for ch in TEXT]
    tracking = TRACKING_EM * FONT_SIZE
    text_w = sum(advances) + tracking * (len(TEXT) - 1)

    bbox = font.getbbox(TEXT)
    text_h = bbox[3] - bbox[1]
    extra = 100 if cursor else 0
    width = int(text_w + PAD * 2 + extra)
    height = int(text_h + PAD * 2)

    image = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)

    x = PAD
    for ch, advance in zip(TEXT, advances):
        draw.text((x, PAD - bbox[1]), ch, font=font, fill=color + (255,))
        x += advance + tracking

    if cursor:
        ch = int(text_h * 0.60)
        cw = int(ch * 0.44)
        x0 = int(PAD + text_w + 34)
        y1 = PAD + text_h
        draw.rounded_rectangle(
            [x0, y1 - ch, x0 + cw, y1],
            radius=max(2, cw // 7),
            fill=AMBER + (255,),
        )
    return image


def make_lockup(icon: Image.Image, wordmark: Image.Image) -> Image.Image:
    icon = icon.resize((160, 160), Image.LANCZOS)
    scale = 150 / wordmark.height
    wordmark = wordmark.resize((int(wordmark.width * scale), 150), Image.LANCZOS)
    gap = 26
    canvas = Image.new("RGBA", (icon.width + gap + wordmark.width, 190), (0, 0, 0, 0))
    canvas.alpha_composite(icon, (0, 15))
    canvas.alpha_composite(wordmark, (icon.width + gap, 20))
    return canvas


def main() -> None:
    ASSETS.mkdir(exist_ok=True)
    dark = render_wordmark(DARK_TEXT, cursor=True)
    light = render_wordmark(LIGHT_TEXT, cursor=True)
    dark.save(ASSETS / "wordmark-dark.png")
    light.save(ASSETS / "wordmark-light.png")

    icon_dark = Image.open(ASSETS / "icon.png").convert("RGBA")
    icon_light = Image.open(ASSETS / "icon-light.png").convert("RGBA")
    make_lockup(icon_dark, dark).save(ASSETS / "lockup-dark.png")
    make_lockup(icon_light, light).save(ASSETS / "lockup-light.png")
    print("wrote assets/wordmark-{dark,light}.png and assets/lockup-{dark,light}.png")


if __name__ == "__main__":
    main()
