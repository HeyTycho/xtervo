#!/usr/bin/env python3
"""文档数字对账：CLOSURE.md 的统计必须与 FEATURE_MATRIX.md 现算一致。

用法：
    python3 scripts/check_docs_counts.py          # 不一致时退出码 1
    python3 scripts/check_docs_counts.py --fix    # 用现算值直接改 CLOSURE 表格

背景（ACT-037）：CLOSURE/ACTION_ITEMS 长期落后于发版，出现过“文档写 23 条、
实际 57 条”的情况。发版流程里加这一步，数字漂移就不会溜过去。
"""
import collections
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
MATRIX = ROOT / "docs" / "FEATURE_MATRIX.md"
CLOSURE = ROOT / "docs" / "CLOSURE.md"
STATUSES = ["implemented", "partial", "needs_verification", "not_started", "unsupported"]


def compute():
    rows = []
    in_app = False
    for line in MATRIX.read_text().splitlines():
        if line.startswith("## 5. 附"):
            in_app = True
            continue
        if line.startswith("## 6."):
            in_app = False
            continue
        if in_app or not line.startswith("| "):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) >= 7 and re.match(r"^[A-Z]+-[A-Z0-9-]+$", cells[0]):
            rows.append(cells)
    counts = collections.Counter(r[4] for r in rows)
    total = len(rows)
    weighted = (
        counts["implemented"] + counts["partial"] * 0.5 + counts["needs_verification"] * 0.25
    ) / max(total, 1) * 100
    return total, counts, weighted


def documented(text):
    """从 CLOSURE §2 表格里读出文档声称的数字。"""
    got = {}
    for key in STATUSES:
        m = re.search(rf"\| {key} \| \*\*(\d+)", text)
        if m:
            got[key] = int(m.group(1))
    m = re.search(r"\| 主条目 \| \*\*(\d+)\*\*", text)
    if m:
        got["total"] = int(m.group(1))
    return got


def main() -> int:
    total, counts, weighted = compute()
    text = CLOSURE.read_text()
    got = documented(text)
    expected = {k: counts[k] for k in STATUSES}
    expected["total"] = total

    bad = {k: (got.get(k), v) for k, v in expected.items() if got.get(k) != v}
    print(f"matrix: total={total} " + " ".join(f"{k}={counts[k]}" for k in STATUSES)
          + f" weighted={weighted:.1f}%")
    print(f"closure: {got}")
    if not bad:
        print("PASS: CLOSURE 与矩阵一致")
        return 0
    for k, (doc, real) in bad.items():
        print(f"DRIFT: {k} 文档={doc} 实际={real}")
    if "--fix" not in sys.argv:
        print("不一致；用 --fix 覆盖，或手工更新 CLOSURE §2/§3")
        return 1

    # --fix：只改 §2 表格里的数字（不碰叙述文字）
    for key, real in expected.items():
        if key == "total":
            text = re.sub(r"(\| 主条目 \| \*\*)\d+(\*\*)", rf"\g<1>{real}\g<2>", text)
        else:
            pct = real / total * 100 if total else 0.0
            text = re.sub(
                rf"(\| {key} \| \*\*)\d+（[0-9.]+%）(\*\*)",
                rf"\g<1>{real}（{pct:.1f}%）\g<2>",
                text,
            )
    text = re.sub(r"(\| 加权分 \| \*\*)[0-9.]+%", rf"\g<1>{weighted:.1f}%", text)
    CLOSURE.write_text(text)
    print("已按现算值更新 CLOSURE §2")
    return 0


if __name__ == "__main__":
    sys.exit(main())
