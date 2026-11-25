#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""生成 xtervo 的项目图标。

设计（2026-11 改版）：扁平单色，无渐变/发光。
深色圆角方块 + 细描边 + 白色几何 "x" + 琥珀色光标线；
另生成浅色版本供字标拼版使用。字标由 scripts/gen_wordmark.py 生成。

输出：
    assets/icon.png        512x512 RGBA（深色）
    assets/icon-light.png  512x512 RGBA（浅色）
    assets/icon-small.png   64x64  RGBA（深色，Slint 标题栏）
    assets/icon-64.rgba     64x64  原始 RGBA（X11 _NET_WM_ICON）
"""
import pathlib
import struct
import zlib

import numpy as np

ROOT = pathlib.Path(__file__).resolve().parent.parent
SS = 4  # 超采样倍数

DARK_PLATE = (0x0B, 0x0D, 0x10)
DARK_BORDER = (0x25, 0x2A, 0x31)
DARK_MARK = (0xF4, 0xF6, 0xF8)
LIGHT_PLATE = (0xF6, 0xF7, 0xF8)
LIGHT_BORDER = (0xD9, 0xDD, 0xE2)
LIGHT_MARK = (0x10, 0x13, 0x17)
AMBER = (0xE8, 0xA3, 0x3D)


def rounded_rect(x, y, x0, y0, x1, y1, r):
    cx = np.clip(x, x0 + r, x1 - r)
    cy = np.clip(y, y0 + r, y1 - r)
    return np.hypot(x - cx, y - cy) <= r


def segment_distance(x, y, x0, y0, x1, y1):
    dx, dy = x1 - x0, y1 - y0
    t = np.clip(((x - x0) * dx + (y - y0) * dy) / (dx * dx + dy * dy), 0.0, 1.0)
    return np.hypot(x - (x0 + t * dx), y - (y0 + t * dy))


def render(size: int, light: bool = False) -> np.ndarray:
    n = size * SS
    ys, xs = np.mgrid[0:n, 0:n].astype(np.float32)
    x = xs / SS
    y = ys / SS
    s = float(size)

    plate = LIGHT_PLATE if light else DARK_PLATE
    border = LIGHT_BORDER if light else DARK_BORDER
    mark = LIGHT_MARK if light else DARK_MARK

    # 扁平底板 + 细描边（无渐变）
    bg_mask = rounded_rect(x, y, 0, 0, s, s, s * 0.16)
    color = np.zeros((n, n, 3), np.float32)
    color[:] = plate
    bw = max(s * 0.006, 0.35)
    inner = rounded_rect(x, y, bw, bw, s - bw, s - bw, s * 0.16 - bw)
    color[bg_mask & ~inner] = border

    # 几何 "x"：两条等粗线段，端点留白一致
    a0, a1 = s * 0.30, s * 0.64
    half = s * 0.0525
    d = np.minimum(
        segment_distance(x, y, a0, a0, a1, a1),
        segment_distance(x, y, a1, a0, a0, a1),
    )
    color[d <= half] = mark

    # 琥珀色光标线，和 x 留出呼吸空间
    caret = rounded_rect(x, y, s * 0.345, s * 0.735, s * 0.655, s * 0.785, s * 0.024)
    color[caret] = AMBER

    rgba = np.zeros((n, n, 4), np.float32)
    rgba[..., :3] = color
    rgba[..., 3] = np.where(bg_mask, 255.0, 0.0)

    rgba = rgba.reshape(size, SS, size, SS, 4).mean(axis=(1, 3))
    return np.clip(rgba, 0, 255).astype(np.uint8)


def png_bytes(rgba: np.ndarray) -> bytes:
    h, w, _ = rgba.shape
    raw = b"".join(b"\x00" + rgba[y].tobytes() for y in range(h))

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    assets = ROOT / "assets"
    assets.mkdir(exist_ok=True)
    dark = render(512)
    light = render(512, light=True)
    small = render(64)
    (assets / "icon.png").write_bytes(png_bytes(dark))
    (assets / "icon-light.png").write_bytes(png_bytes(light))
    (assets / "icon-small.png").write_bytes(png_bytes(small))
    (assets / "icon-64.rgba").write_bytes(small.tobytes())
    print(
        "wrote assets/icon.png (512), icon-light.png (512), "
        "icon-small.png (64), icon-64.rgba (64)"
    )


if __name__ == "__main__":
    main()
