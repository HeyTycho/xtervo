#!/usr/bin/env python3
"""把 xwd 截图转成 PNG，便于人工/Agent 做视觉验收（无需 ImageMagick）。

用法：
    xwd -id <window> -silent -out shot.xwd
    python3 scripts/xwd2png.py shot.xwd shot.png

只处理 24/32 位 TrueColor、4 字节/像素的 xwd（本机 Xvfb/Xorg 都是这种）。
"""
import pathlib
import struct
import sys
import zlib


def convert(src: pathlib.Path, dst: pathlib.Path) -> None:
    data = src.read_bytes()
    header = struct.unpack(">I", data[0:4])[0]
    width = struct.unpack(">I", data[16:20])[0]
    height = struct.unpack(">I", data[20:24])[0]
    bytes_per_line = struct.unpack(">I", data[48:52])[0]
    ncolors = struct.unpack(">I", data[76:80])[0]
    offset = header + ncolors * 12  # XWDCOLOR 条目 12 字节

    rows = []
    for y in range(height):
        line = data[offset + y * bytes_per_line: offset + y * bytes_per_line + width * 4]
        rgb = bytearray()
        for x in range(width):
            b, g, r, _ = line[x * 4: x * 4 + 4]
            rgb += bytes((r, g, b))
        rows.append(bytes(rgb))
    raw = b"".join(b"\x00" + r for r in rows)

    def chunk(kind: bytes, payload: bytes) -> bytes:
        body = kind + payload
        return struct.pack(">I", len(payload)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 6))
        + chunk(b"IEND", b"")
    )
    dst.write_bytes(png)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    convert(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]))
    print(f"wrote {sys.argv[2]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
