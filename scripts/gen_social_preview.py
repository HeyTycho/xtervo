#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""生成 GitHub Social Preview（1280x640）。

GitHub 推荐 1280x640、<1 MB。输出深/浅两版：
    assets/social-preview.png        深色（默认上传这张）
    assets/social-preview-light.png  浅色
需要 Pillow（仅生成资产用）。
"""
import pathlib
import sys

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
W, H = 1280, 640

DARK_BG = (0x0B, 0x0D, 0x10)
LIGHT_BG = (0xF6, 0xF7, 0xF8)
DARK_TEXT = (0x9A, 0xA4, 0xB2)
DARK_SUB = (0x62, 0x6C, 0x7A)
LIGHT_TEXT = (0x3A, 0x42, 0x4D)
LIGHT_SUB = (0x6B, 0x74, 0x80)
AMBER = (0xE8, 0xA3, 0x3D)

TAGLINE = "A fast, light native terminal for Linux"
SUBLINE = "Rust + Slint  ·  GPU with software fallback  ·  SSH / Serial / Telnet"

FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
]


def font(size: int) -> ImageFont.FreeTypeFont:
    for path in FONT_CANDIDATES:
        if pathlib.Path(path).is_file():
            return ImageFont.truetype(path, size)
    return ImageFont.load_default()


def centered(draw, text, y, f, fill):
    bbox = draw.textbbox((0, 0), text, font=f)
    draw.text(((W - (bbox[2] - bbox[0])) / 2, y), text, font=f, fill=fill)


def compose(light: bool) -> Image.Image:
    bg = LIGHT_BG if light else DARK_BG
    image = Image.new("RGB", (W, H), bg)
    draw = ImageDraw.Draw(image)

    lockup = Image.open(ASSETS / ("lockup-light.png" if light else "lockup-dark.png")).convert("RGBA")
    scale = 720 / lockup.width
    lockup = lockup.resize((720, int(lockup.height * scale)), Image.LANCZOS)
    image.paste(lockup, ((W - lockup.width) // 2, 190), lockup)

    centered(draw, TAGLINE, 420, font(28), LIGHT_TEXT if light else DARK_TEXT)
    centered(draw, SUBLINE, 474, font(20), LIGHT_SUB if light else DARK_SUB)

    # 一条很克制的琥珀短线，呼应光标
    bar_w, bar_h = 64, 4
    draw.rounded_rectangle(
        [(W - bar_w) // 2, 540, (W + bar_w) // 2, 540 + bar_h],
        radius=2,
        fill=AMBER,
    )
    return image


def main() -> None:
    if not (ASSETS / "lockup-dark.png").is_file():
        print("缺少 assets/lockup-*.png；先运行 scripts/gen_icon.py 和 scripts/gen_wordmark.py", file=sys.stderr)
        raise SystemExit(1)
    dark = compose(False)
    light = compose(True)
    dark.save(ASSETS / "social-preview.png", optimize=True)
    light.save(ASSETS / "social-preview-light.png", optimize=True)
    for path in ("social-preview.png", "social-preview-light.png"):
        size = (ASSETS / path).stat().st_size
        print(f"wrote assets/{path} (1280x640, {size / 1024:.1f} KiB)")


if __name__ == "__main__":
    main()
