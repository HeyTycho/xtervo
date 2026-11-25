#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""生成 xtervo 的项目图标（原创几何标记，无任何猫/终端窗口品牌元素）。

设计：深色圆角方块 + 圆角描边 + 青→蓝渐变的 "T" 字母 + 琥珀色光标块。
输出：
    assets/icon.png       512x512 RGBA
    assets/icon-small.png  64x64  RGBA（Slint 标题栏）
    assets/icon-64.rgba    64x64  原始 RGBA（X11 _NET_WM_ICON）
"""
import pathlib
import struct
import zlib

import numpy as np

ROOT = pathlib.Path(__file__).resolve().parent.parent
SS = 4  # 超采样倍数


def rounded_rect(x, y, x0, y0, x1, y1, r):
    cx = np.clip(x, x0 + r, x1 - r)
    cy = np.clip(y, y0 + r, y1 - r)
    return np.hypot(x - cx, y - cy) <= r


def lerp(a, b, t):
    return np.asarray(a, np.float32) + (np.asarray(b, np.float32) - np.asarray(a, np.float32)) * t


def render(size: int) -> np.ndarray:
    n = size * SS
    ys, xs = np.mgrid[0:n, 0:n].astype(np.float32)
    x = xs / SS
    y = ys / SS
    s = float(size)

    # 背景：深色圆角方块 + 垂直渐变
    bg_mask = rounded_rect(x, y, 0, 0, s, s, s * 0.22)
    t = (y / s)[..., None]
    top = np.array([0x12, 0x17, 0x1F], np.float32)
    bottom = np.array([0x0A, 0x0D, 0x12], np.float32)
    color = top + (bottom - top) * t

    # 内描边（细线，增加层次）
    outer = rounded_rect(x, y, s * 0.02, s * 0.02, s * 0.98, s * 0.98, s * 0.21)
    inner = rounded_rect(x, y, s * 0.045, s * 0.045, s * 0.955, s * 0.955, s * 0.195)
    border = outer & ~inner
    color[border] = np.array([0x2A, 0x35, 0x42], np.float32)

    # "T"：横杠 + 竖干，青→蓝渐变
    bar = rounded_rect(x, y, s * 0.265, s * 0.275, s * 0.735, s * 0.375, s * 0.05)
    stem = rounded_rect(x, y, s * 0.448, s * 0.345, s * 0.552, s * 0.675, s * 0.05)
    tmask = bar | stem
    gt = ((x - s * 0.265) / (s * 0.47))[..., None]
    grad = lerp((0x37, 0xE0, 0xC9), (0x3B, 0x82, 0xF6), np.clip(gt, 0, 1))
    color[tmask] = grad[tmask]

    # 光标块（琥珀色），放在 T 脚右侧，像终端里的 caret
    caret = rounded_rect(x, y, s * 0.60, s * 0.72, s * 0.735, s * 0.795, s * 0.035)
    color[caret] = np.array([0xFF, 0xB4, 0x54], np.float32)

    rgba = np.zeros((n, n, 4), np.float32)
    rgba[..., :3] = color
    rgba[..., 3] = np.where(bg_mask, 255.0, 0.0)

    # 超采样平均
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
    big = render(512)
    small = render(64)
    (assets / "icon.png").write_bytes(png_bytes(big))
    (assets / "icon-small.png").write_bytes(png_bytes(small))
    (assets / "icon-64.rgba").write_bytes(small.tobytes())
    print("wrote assets/icon.png (512), icon-small.png (64), icon-64.rgba (64)")


if __name__ == "__main__":
    main()
