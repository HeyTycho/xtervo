#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""统计 docs/FEATURE_MATRIX.md 的状态分布，并检查 implemented 行是否有证据。

用法：python3 scripts/matrix_stats.py [--list-implemented]
"""
import collections
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
MATRIX = ROOT / "docs" / "FEATURE_MATRIX.md"
STATUSES = {"implemented", "partial", "not_started", "needs_verification", "unsupported", "blocked"}


def parse():
    """返回 (主条目, 附录条目)。附录是“非复选框，不计入 228 条”的补充表。"""
    rows, extra = [], []
    in_appendix = False
    for line in MATRIX.read_text().splitlines():
        if line.startswith("## 5. 附"):
            in_appendix = True
            continue
        if line.startswith("## 6."):
            in_appendix = False
            continue
        if not line.startswith("| ") or line.startswith("| ID ") or line.startswith("|---"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) >= 7 and re.match(r"^[A-Z]+-[A-Z0-9-]+$", cells[0]):
            (extra if in_appendix else rows).append(cells)
    return rows, extra


def main() -> int:
    rows, extra = parse()
    counts = collections.Counter(r[4] for r in rows)
    print(f"rows: {len(rows)} (appendix {len(extra)}, 不计入完成度)")
    for k in sorted(counts):
        print(f"  {k:18} {counts[k]}")

    bad = [r for r in rows if r[4] not in STATUSES]
    if bad:
        print("!! 非法状态:", [r[0] for r in bad])
        return 1

    score = (counts["implemented"] + counts["partial"] * 0.5 + counts["needs_verification"] * 0.25) / max(len(rows), 1)
    print(f"weighted: {score * 100:.1f}%")

    impl = [r for r in rows if r[4] == "implemented"]
    weak = [r for r in impl if len(r[5]) < 8 or "无" == r[5].strip()]
    print(f"implemented: {len(impl)}, 证据过短: {len(weak)}")
    for r in weak:
        print(f"  !! {r[0]} 证据={r[5]!r}")

    if "--list-implemented" in sys.argv:
        for r in impl:
            print(f"{r[0]:12} {r[5][:80]}")
    return 0 if not bad and not weak else 1


if __name__ == "__main__":
    raise SystemExit(main())
