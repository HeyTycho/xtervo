#!/usr/bin/env python3
"""把 244 条矩阵按“产品必要性”分档（范围审视，2026-11-17）。

矩阵本身是**上游 Tabby 能力审计**（尽量全），不等于产品必须清单。
这里按“对 Tervo 这个终端产品的价值”重新分档：

    P0  核心：不做就不是一个好用的终端
    P1  应该有：工作区/设置/通知等完整体验
    P2  高级：连接后端进阶、嵌入式场景
    P3  不建议/观察：生态兼容重赌注、条件项、明确不做的依赖

用法：
    python3 scripts/scope_tiers.py            # 打印表格
    python3 scripts/scope_tiers.py --ids P3   # 列出某档条目
"""

from __future__ import annotations

import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# 顺序敏感：先匹配到的档位生效。
RULES: list[tuple[str, tuple[str, ...]]] = [
    # 生态兼容重赌注：Node sidecar + Angular WebView，投入大、与 v3 插件路线冲突
    ("P3 不建议/观察", ("PLUG-007", "PLUG-008", "PLUG-009", "PLUG-010", "PLUG-011", "PLUG-012", "PLUG-013")),
    ("P3 不建议/观察", ("ZMODEM-", "UI-SER-008", "THEME-007", "UI-QUAKE-")),
    # 平台/发布：按目标平台决定，不阻塞核心体验
    ("P3 不建议/观察", ("PLAT-",)),
    # 高级连接：ProxyJump/X11/Login Scripts/算法、串口 Hex/换行/Zmodem
    ("P2 高级连接", ("SSH-006", "SSH-008", "SSH-010", "SSH-012", "SSH-013", "SSH-014", "UI-SSH-008")),
    ("P2 高级连接", ("SERIAL-004", "SERIAL-005", "SERIAL-006", "SERIAL-009", "SERIAL-010", "TELNET-004", "UI-SER-")),
    # 核心终端
    ("P0 核心终端", ("TERM-", "UI-TERM-", "KEY-", "UI-PANE-", "UI-TAB-", "UI-WIN-", "LOCAL-", "SESS-")),
    # 基础连接后端（SSH/Serial/Telnet 主干 + SFTP）
    ("P0 基础连接", ("SSH-001", "SSH-002", "SSH-003", "SSH-004", "SSH-005", "SSH-015")),
    ("P0 基础连接", ("SERIAL-001", "SERIAL-002", "SERIAL-003", "SERIAL-007", "SERIAL-008")),
    ("P0 基础连接", ("TELNET-001", "TELNET-002", "TELNET-003", "SFTP-")),
    # 工作区/设置/通知/安全/诊断
    ("P1 工作区与设置", ("UI-NAV-", "UI-PROF-", "UI-SET-", "UI-PLUG-", "NOTIFY-", "SEC-", "OPS-", "UI-SSH-")),
    ("P1 工作区与设置", ("THEME-",)),
]

FALLBACK = "P2 高级连接"
TIER_ORDER = ["P0 核心终端", "P0 基础连接", "P1 工作区与设置", "P2 高级连接", "P3 不建议/观察"]
STATUSES = ["implemented", "partial", "needs_verification", "not_started", "unsupported"]


def tier_of(item_id: str) -> str:
    for tier, prefixes in RULES:
        if any(item_id.startswith(p) for p in prefixes):
            return tier
    return FALLBACK


def load_matrix() -> list[tuple[str, str, str]]:
    """返回 (id, feature, status)。口径与 scripts/matrix_stats.py 一致。"""
    text = (ROOT / "docs/FEATURE_MATRIX.md").read_text(encoding="utf-8")
    rows: list[tuple[str, str, str]] = []
    in_appendix = False
    for line in text.splitlines():
        if line.startswith("## 5. 附"):
            in_appendix = True
            continue
        if line.startswith("## 6."):
            in_appendix = False
            continue
        if in_appendix or not line.startswith("| "):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) >= 7 and re.match(r"^[A-Z]+-[A-Z0-9-]+$", cells[0]):
            rows.append((cells[0], cells[1], cells[4]))
    return rows


def stats(rows: list[tuple[str, str, str]]) -> dict[str, dict]:
    out: dict[str, dict] = {tier: Counter() for tier in TIER_ORDER}
    for item_id, _feature, status in rows:
        out[tier_of(item_id)][status] += 1
        out[tier_of(item_id)]["total"] += 1
    return out


def weighted(counter: Counter) -> float:
    total = sum(counter.get(k, 0) for k in ("implemented", "partial", "needs_verification", "not_started", "unsupported"))
    if not total:
        return 0.0
    score = (
        counter.get("implemented", 0)
        + counter.get("partial", 0) * 0.5
        + counter.get("needs_verification", 0) * 0.25
    ) / total
    return score * 100


def main() -> int:
    rows = load_matrix()
    per_tier = stats(rows)
    print(f"total {len(rows)}")
    print()
    print("| 档位 | 条目 | 已实现 | 部分 | 待核实 | 未开始 | 不做 | 加权 |")
    print("|---|---|---|---|---|---|---|---|")
    for tier in TIER_ORDER:
        c = per_tier[tier]
        print(
            f"| {tier} | {c['total']} | {c.get('implemented',0)} | {c.get('partial',0)} | "
            f"{c.get('needs_verification',0)} | {c.get('not_started',0)} | {c.get('unsupported',0)} | {weighted(c):.1f}% |"
        )
    core = per_tier["P0 核心终端"] + per_tier["P0 基础连接"]
    core_total = core["total"]
    core_done = core.get("implemented", 0) + core.get("partial", 0) * 0.5 + core.get("needs_verification", 0) * 0.25
    print()
    print(f"P0 合计 {core_total} 条，加权 {core_done / max(core_total,1) * 100:.1f}%")
    scope = sum(per_tier[t]["total"] for t in ("P0 核心终端", "P0 基础连接", "P1 工作区与设置"))
    scope_done = sum(
        per_tier[t].get("implemented", 0) + per_tier[t].get("partial", 0) * 0.5 + per_tier[t].get("needs_verification", 0) * 0.25
        for t in ("P0 核心终端", "P0 基础连接", "P1 工作区与设置")
    )
    print(f"P0+P1 合计 {scope} 条（占 {scope/len(rows)*100:.0f}%），加权 {scope_done/scope*100:.1f}%")

    if "--ids" in sys.argv:
        want = sys.argv[sys.argv.index("--ids") + 1]
        for item_id, feature, status in rows:
            if tier_of(item_id).startswith(want):
                print(f"{item_id:12} {status:18} {feature[:70]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
