#!/usr/bin/env python3
"""全屏重绘压力源：以固定帧率向终端输出整屏 ANSI 帧。

用于 Phase 0 的 "vim/htop 全屏刷新 60 FPS" 等价压测：
每一帧都是整屏变化，逼 UI 走完整的光栅化 + 上传路径。
"""
import argparse
import random
import sys
import time

PALETTE = "01234567"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=32)
    ap.add_argument("--seconds", type=float, default=10.0)
    ap.add_argument("--fps", type=float, default=60.0)
    args = ap.parse_args()

    rng = random.Random(42)
    alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
    width = max(8, args.cols)

    out = sys.stdout
    out.write("\x1b[2J")
    deadline = time.monotonic() + args.seconds
    period = 1.0 / args.fps
    next_at = time.monotonic()

    while time.monotonic() < deadline:
        lines = []
        for row in range(args.rows):
            body = "".join(rng.choice(alphabet) for _ in range(width - 8))
            lines.append(
                "\x1b[3%dm%2d %s" % (row % 8, row, body[: width - 4])
            )
        out.write("\x1b[H" + "\r\n".join(lines))
        out.flush()
        next_at += period
        # 混合睡眠：粗睡后忙等，降低源端抖动，避免把源的不准算到渲染头上
        while True:
            remain = next_at - time.monotonic()
            if remain <= 0:
                break
            if remain > 0.0015:
                time.sleep(remain - 0.0012)
    return 0


if __name__ == "__main__":
    sys.exit(main())
