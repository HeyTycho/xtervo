#!/usr/bin/env python3
"""检查仓库内 Markdown 相对链接是否都能解析到真实文件。

用法：python3 scripts/check_doc_links.py [--fix]
  --fix  仅报告（保留参数位，便于未来自动改写）
"""
from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LINK = re.compile(r"\]\(([^)#\s]+)(#[^)]*)?\)")
SKIP_PREFIX = ("http://", "https://", "mailto:", "#")


def main() -> int:
    broken: list[tuple[pathlib.Path, str, str]] = []
    for md in sorted(ROOT.rglob("*.md")):
        try:
            rel = md.relative_to(ROOT)
        except ValueError:
            continue
        if any(part.startswith(".") or part in {"target", "node_modules", "dist", "vendor", "dist-preview"} for part in rel.parts):
            continue
        try:
            text = md.read_text(errors="replace")
        except OSError:
            continue
        for match in LINK.finditer(text):
            target = match.group(1)
            if target.startswith(SKIP_PREFIX):
                continue
            # 去掉 `<...>` 与行内锚点
            pure = target.strip("<>").split("#", 1)[0]
            if not pure:
                continue
            if pure.startswith("/"):
                resolved = ROOT / pure.lstrip("/")
            else:
                resolved = (md.parent / pure).resolve()
            try:
                resolved.relative_to(ROOT.resolve())
            except ValueError:
                continue  # 仓库外链接不检查
            if not resolved.exists():
                broken.append((md.relative_to(ROOT), target, str(resolved.relative_to(ROOT))))
    for src, target, resolved in broken:
        print(f"BROKEN {src}: {target} -> {resolved}")
    print(f"checked: broken={len(broken)}")
    return 1 if broken else 0


if __name__ == "__main__":
    raise SystemExit(main())
