#!/usr/bin/env python3
"""把路标 / 产品矩阵 / 今晚计划 / 最近提交生成单文件网页。

用法：
    python3 scripts/gen_status_page.py [--out .logs/status.html]

数据源（全部现读，不手工抄数字）：
    docs/FEATURE_MATRIX.md    产品矩阵 244 条
    docs/ACTION_ITEMS.md §8   0.6 路标（ACT 卡片）
    docs/EVENING_START.md     F1..F11 今晚计划
    git log                   最近提交
    scripts/matrix_stats.py   官方口径的计数与加权完成度
"""

from __future__ import annotations

import html
import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STATUS_ORDER = ["implemented", "partial", "needs_verification", "not_started", "unsupported"]
STATUS_ZH = {
    "implemented": "已实现",
    "partial": "部分",
    "needs_verification": "待核实",
    "not_started": "未开始",
    "unsupported": "不做",
}


def split_row(line: str) -> list[str]:
    return [cell.strip() for cell in line.strip().strip("|").split("|")]


def norm_status(raw: str) -> str:
    low = raw.lower().strip()
    for status in STATUS_ORDER:
        if low.startswith(status):
            return status
    return "partial" if raw else "not_started"


def parse_matrix() -> list[dict]:
    """与 scripts/matrix_stats.py 同口径：`## 5. 附…` 起的附录表不计入完成度。"""
    text = (ROOT / "docs/FEATURE_MATRIX.md").read_text(encoding="utf-8")
    rows: list[dict] = []
    in_appendix = False
    pattern = re.compile(r"^\|\s*([A-Z]+-[A-Z0-9-]+)\s*\|")
    for line in text.splitlines():
        if line.startswith("## 5. 附"):
            in_appendix = True
            continue
        if line.startswith("## 6."):
            in_appendix = False
            continue
        if in_appendix or not pattern.match(line):
            continue
        cells = split_row(line)
        if len(cells) < 7:
            continue
        rows.append(
            {
                "id": cells[0],
                "feature": cells[1],
                "status": norm_status(cells[4]),
                "test": cells[5],
                "limitation": cells[7] if len(cells) > 7 else "",
                "area": cells[0].rsplit("-", 1)[0],
            }
        )
    return rows


def parse_action_items() -> list[dict]:
    text = (ROOT / "docs/ACTION_ITEMS.md").read_text(encoding="utf-8")
    sections: list[dict] = []
    current: dict | None = None
    for line in text.splitlines():
        if line.startswith("### 8."):
            current = {"title": line.lstrip("# ").strip(), "cards": []}
            sections.append(current)
        elif current is not None and line.startswith("| ACT-"):
            cells = split_row(line)
            if len(cells) >= 5:
                current["cards"].append(
                    {
                        "id": cells[0],
                        "task": cells[1],
                        "covers": cells[2] if len(cells) >= 6 else "",
                        "state": cells[3] if len(cells) >= 6 else "",
                        "accept": cells[4] if len(cells) >= 6 else "",
                        "size": cells[-1],
                    }
                )
        elif line.startswith("## ") and not line.startswith("### 8."):
            current = None
    return [s for s in sections if s["cards"]]


def parse_plan() -> list[dict]:
    text = (ROOT / "docs/EVENING_START.md").read_text(encoding="utf-8")
    plan: list[dict] = []
    for line in text.splitlines():
        if re.match(r"^\|\s*F\d+\s*\|", line):
            cells = split_row(line)
            if len(cells) >= 4:
                plan.append({"id": cells[0], "task": cells[1], "where": cells[2], "eta": cells[3]})
    return plan


def official_stats(matrix: list[dict]) -> tuple[dict[str, int], str]:
    """与 scripts/matrix_stats.py 相同的加权公式，直接用已解析的行算。"""
    counts: dict[str, int] = {}
    for row in matrix:
        counts[row["status"]] = counts.get(row["status"], 0) + 1
    total = len(matrix)
    counts["total"] = total
    score = (
        counts.get("implemented", 0)
        + counts.get("partial", 0) * 0.5
        + counts.get("needs_verification", 0) * 0.25
    ) / max(total, 1)
    return counts, f"{score * 100:.1f}%"


def git_log(limit: int = 18) -> list[dict]:
    try:
        out = subprocess.run(
            ["git", "log", f"--pretty=format:%h\x1f%ad\x1f%s", "--date=short", f"-{limit}"],
            capture_output=True,
            text=True,
            cwd=ROOT,
            timeout=30,
        ).stdout
    except Exception:
        return []
    commits = []
    for line in out.splitlines():
        parts = line.split("\x1f")
        if len(parts) == 3:
            commits.append({"hash": parts[0], "date": parts[1], "subject": parts[2]})
    return commits


def git_facts() -> dict[str, str]:
    def run(args: list[str], default: str = "—") -> str:
        try:
            return subprocess.run(
                args, capture_output=True, text=True, cwd=ROOT, timeout=30
            ).stdout.strip() or default
        except Exception:
            return default

    return {
        "head": run(["git", "log", "-1", "--pretty=%h %s"]),
        "tag": run(["git", "describe", "--tags", "--always", "--abbrev=7"], "no tag"),
        "branch": run(["git", "rev-parse", "--abbrev-ref", "HEAD"]),
        "dirty": run(["git", "status", "--porcelain"]) or "干净",
        "count": run(["git", "rev-list", "--count", "v0.5.0..HEAD"], "?"),
    }


def clean_md(text: str) -> str:
    """去掉 `**粗体**`、反引号、Markdown 链接，只留可读文本。"""
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)
    text = text.replace("**", "").replace("`", "")
    return text.strip()


def esc(text: str) -> str:
    return html.escape(text, quote=True)


def tone(status: str) -> str:
    return {
        "implemented": "ok",
        "partial": "warn",
        "needs_verification": "info",
        "not_started": "idle",
        "unsupported": "bad",
    }.get(status, "idle")


def build_html() -> str:
    matrix = parse_matrix()
    sections = parse_action_items()
    plan = parse_plan()
    counts, weighted = official_stats(matrix)
    commits = git_log()
    facts = git_facts()
    now = datetime.now().strftime("%Y-%m-%d %H:%M")

    total = counts.get("total") or len(matrix)
    implemented = counts.get("implemented", 0)
    partial = counts.get("partial", 0)
    not_started = counts.get("not_started", 0)
    needs_verif = counts.get("needs_verification", 0)
    unsupported = counts.get("unsupported", 0)
    weighted = weighted or f"{implemented / max(total, 1) * 100:.1f}%"

    areas: dict[str, dict[str, int]] = {}
    for row in matrix:
        bucket = areas.setdefault(row["area"], {})
        bucket[row["status"]] = bucket.get(row["status"], 0) + 1

    def card_html(section: dict) -> str:
        cards = []
        for card in section["cards"]:
            state = esc(clean_md(card["state"]))
            tone_class = (
                "ok"
                if ("已做" in state or "已完成" in state or "已落地" in state or "已补" in state)
                else ("idle" if ("缺" in state or "未做" in state or "无" == state.strip()) else "warn")
            )
            cards.append(
                f"""
        <article class="card {tone_class}">
          <header><code>{esc(card['id'])}</code><span class="size">{esc(card['size'])}</span></header>
          <h3>{esc(clean_md(card["task"]))}</h3>
          <p class="muted">覆盖：{esc(card['covers'])}</p>
          <p>{state}</p>
          <p class="muted small">验收：{esc(clean_md(card["accept"])[:220])}{'…' if len(clean_md(card['accept'])) > 220 else ''}</p>
        </article>"""
            )
        return f"""
      <section class="track">
        <h2>{esc(section['title'])}</h2>
        <div class="grid">{''.join(cards)}</div>
      </section>"""

    plan_rows = "".join(
        f"<tr><td><code>{esc(p['id'])}</code></td><td>{esc(p['task'])}</td>"
        f"<td class='muted'>{esc(p['where'])}</td><td>{esc(p['eta'])}</td></tr>"
        for p in plan
    )

    matrix_rows = "".join(
        f"""<tr data-status="{row['status']}" data-search="{esc((row['id'] + ' ' + row['feature'] + ' ' + row['limitation']).lower())}">
          <td><code>{esc(row['id'])}</code></td>
          <td>{esc(row['feature'])}</td>
          <td><span class="badge {tone(row['status'])}">{STATUS_ZH.get(row['status'], row['status'])}</span></td>
          <td class="muted small">{esc(clean_md(row['limitation'])[:180])}{'…' if len(clean_md(row['limitation'])) > 180 else ''}</td>
        </tr>"""
        for row in matrix
    )

    area_rows = "".join(
        f"<tr><td><code>{esc(area)}</code></td>"
        + "".join(
            f"<td>{bucket.get(status, 0)}</td>" for status in STATUS_ORDER
        )
        + f"<td>{sum(bucket.values())}</td></tr>"
        for area, bucket in sorted(areas.items(), key=lambda kv: -sum(kv[1].values()))
    )

    commit_rows = "".join(
        f"<li><code>{esc(c['hash'])}</code><span class='muted'>{esc(c['date'])}</span> {esc(c['subject'])}</li>"
        for c in commits
    )

    chips = "".join(
        f"<button class='chip {tone(s)}' data-filter='{s}'>{STATUS_ZH[s]} <b>{counts.get(s, 0)}</b></button>"
        for s in STATUS_ORDER
    )

    return f"""<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Tervo 路标 · 矩阵 · 进展</title>
<style>
  :root {{ color-scheme: dark; --bg:#0f1115; --panel:#171a21; --line:#262b36; --fg:#e6e8ee;
           --muted:#8b93a3; --ok:#3fb950; --warn:#d29922; --info:#58a6ff; --idle:#6e7681; --bad:#f85149; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; background:var(--bg); color:var(--fg);
         font:14px/1.6 -apple-system,"Segoe UI","Noto Sans SC",sans-serif; }}
  header.top {{ position:sticky; top:0; z-index:5; padding:18px 28px 14px;
                background:linear-gradient(180deg,#0f1115 70%,transparent); border-bottom:1px solid var(--line); }}
  h1 {{ margin:0 0 4px; font-size:20px; letter-spacing:.3px; }}
  .sub {{ color:var(--muted); font-size:12px; }}
  main {{ padding:20px 28px 60px; max-width:1500px; }}
  .kpis {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:12px; margin:14px 0 22px; }}
  .kpi {{ background:var(--panel); border:1px solid var(--line); border-radius:10px; padding:12px 14px; }}
  .kpi b {{ display:block; font-size:26px; line-height:1.2; }}
  .kpi span {{ color:var(--muted); font-size:12px; }}
  nav.tabs {{ display:flex; gap:6px; flex-wrap:wrap; margin:6px 0 18px; }}
  nav.tabs button {{ background:var(--panel); color:var(--fg); border:1px solid var(--line);
                     border-radius:8px; padding:7px 13px; cursor:pointer; }}
  nav.tabs button.active {{ border-color:#3b82f6; background:#1b2433; }}
  section.pane {{ display:none; }} section.pane.active {{ display:block; }}
  .track {{ margin-bottom:26px; }}
  .track h2 {{ font-size:15px; color:#c9d1d9; border-left:3px solid #3b82f6; padding-left:9px; }}
  .grid {{ display:grid; grid-template-columns:repeat(auto-fill,minmax(340px,1fr)); gap:12px; }}
  .card {{ background:var(--panel); border:1px solid var(--line); border-left:4px solid var(--idle);
           border-radius:10px; padding:12px 14px; }}
  .card.ok {{ border-left-color:var(--ok); }} .card.warn {{ border-left-color:var(--warn); }}
  .card header {{ display:flex; justify-content:space-between; color:var(--muted); font-size:12px; }}
  .card h3 {{ font-size:14px; margin:6px 0 8px; }}
  .card p {{ margin:4px 0; }}
  .size {{ color:#c9d1d9; }}
  .muted {{ color:var(--muted); }} .small {{ font-size:12px; }}
  table {{ width:100%; border-collapse:collapse; background:var(--panel); border:1px solid var(--line); border-radius:10px; overflow:hidden; }}
  th, td {{ padding:8px 10px; border-bottom:1px solid var(--line); vertical-align:top; text-align:left; }}
  th {{ position:sticky; top:0; background:#1b1f27; font-size:12px; color:#c9d1d9; }}
  tr:last-child td {{ border-bottom:none; }}
  code {{ background:#0b0d11; border:1px solid var(--line); border-radius:5px; padding:1px 5px; font-size:12px; }}
  .badge {{ border-radius:999px; padding:1px 9px; font-size:12px; border:1px solid var(--line); }}
  .badge.ok {{ color:var(--ok); border-color:#1f6f34; }} .badge.warn {{ color:var(--warn); border-color:#7a5b12; }}
  .badge.info {{ color:var(--info); border-color:#1f4f8a; }} .badge.idle {{ color:var(--idle); }}
  .badge.bad {{ color:var(--bad); border-color:#7a2420; }}
  .toolbar {{ display:flex; gap:10px; align-items:center; flex-wrap:wrap; margin:0 0 12px; }}
  input[type=search] {{ background:var(--panel); color:var(--fg); border:1px solid var(--line);
                        border-radius:8px; padding:8px 11px; width:320px; }}
  .chip {{ background:var(--panel); color:var(--fg); border:1px solid var(--line); border-radius:999px; padding:5px 11px; cursor:pointer; }}
  .chip.active {{ outline:2px solid #3b82f6; }}
  .chip.ok {{ color:var(--ok); }} .chip.warn {{ color:var(--warn); }} .chip.info {{ color:var(--info); }}
  .chip.idle {{ color:var(--idle); }} .chip.bad {{ color:var(--bad); }}
  ul.commits {{ list-style:none; padding:0; margin:0; }}
  ul.commits li {{ padding:7px 0; border-bottom:1px solid var(--line); }}
  ul.commits span {{ margin-left:8px; margin-right:8px; font-size:12px; }}
  .facts code {{ margin-right:6px; }}
</style>
</head>
<body>
<header class="top">
  <h1>Tervo · 路标 / 产品矩阵 / 工作进展</h1>
  <div class="sub">生成时间 {esc(now)} · 分支 <code>{esc(facts['branch'])}</code> ·
    HEAD <code>{esc(facts['head'])}</code> · tag <code>{esc(facts['tag'])}</code> ·
    v0.5.0 之后 <b>{esc(facts['count'])}</b> 个提交 · 工作树 {esc(facts['dirty'])[:60]}</div>
</header>
<main>
  <div class="kpis">
    <div class="kpi"><b>{total}</b><span>矩阵条目（官方口径）</span></div>
    <div class="kpi"><b style="color:var(--ok)">{implemented}</b><span>已实现</span></div>
    <div class="kpi"><b style="color:var(--warn)">{partial}</b><span>部分实现</span></div>
    <div class="kpi"><b style="color:var(--idle)">{not_started}</b><span>未开始</span></div>
    <div class="kpi"><b>{needs_verif}</b><span>待核实</span></div>
    <div class="kpi"><b>{weighted}</b><span>加权完成度</span></div>
  </div>

  <nav class="tabs">
    <button class="active" data-pane="road">🗺 0.6 路标（{sum(len(s['cards']) for s in sections)} 张卡）</button>
    <button data-pane="plan">🌙 今晚计划（{len(plan)} 项）</button>
    <button data-pane="matrix">📊 产品矩阵（{len(matrix)} 条）</button>
    <button data-pane="commits">🧾 最近提交（{len(commits)}）</button>
  </nav>

  <section class="pane active" id="pane-road">
    {''.join(card_html(s) for s in sections)}
  </section>

  <section class="pane" id="pane-plan">
    <p class="muted">功能优先、测试后置；每项独立 commit，<code>cargo check</code> 与既有测试必须绿。</p>
    <table>
      <thead><tr><th>#</th><th>功能</th><th>落点</th><th>预估</th></tr></thead>
      <tbody>{plan_rows}</tbody>
    </table>
  </section>

  <section class="pane" id="pane-matrix">
    <div class="toolbar">
      <input type="search" id="q" placeholder="搜索 ID / 功能 / 限制…">
      <button class="chip active" data-filter="all">全部 <b>{len(matrix)}</b></button>
      {chips}
    </div>
    <table id="matrix">
      <thead><tr><th style="width:150px">ID</th><th>功能</th><th style="width:92px">状态</th><th>限制</th></tr></thead>
      <tbody>{matrix_rows}</tbody>
    </table>
    <h3 style="margin-top:22px">按模块统计</h3>
    <table>
      <thead><tr><th>模块</th><th>已实现</th><th>部分</th><th>待核实</th><th>未开始</th><th>不做</th><th>合计</th></tr></thead>
      <tbody>{area_rows}</tbody>
    </table>
  </section>

  <section class="pane" id="pane-commits">
    <ul class="commits">{commit_rows}</ul>
  </section>
</main>
<script>
  const tabs = document.querySelectorAll('nav.tabs button');
  const panes = document.querySelectorAll('section.pane');
  tabs.forEach(b => b.onclick = () => {{
    tabs.forEach(x => x.classList.toggle('active', x === b));
    panes.forEach(p => p.classList.toggle('active', p.id === 'pane-' + b.dataset.pane));
  }});

  const q = document.getElementById('q');
  const rows = Array.from(document.querySelectorAll('#matrix tbody tr'));
  const chips = Array.from(document.querySelectorAll('.chip[data-filter]'));
  let filter = 'all';
  function apply() {{
    const text = (q && q.value || '').trim().toLowerCase();
    rows.forEach(r => {{
      const okText = !text || (r.dataset.search || '').includes(text);
      const okStatus = filter === 'all' || r.dataset.status === filter;
      r.style.display = okText && okStatus ? '' : 'none';
    }});
  }}
  if (q) q.addEventListener('input', apply);
  chips.forEach(c => c.onclick = () => {{
    filter = c.dataset.filter;
    chips.forEach(x => x.classList.toggle('active', x === c));
    apply();
  }});
</script>
</body>
</html>
"""


def main() -> int:
    out = Path(sys.argv[sys.argv.index("--out") + 1]) if "--out" in sys.argv else ROOT / ".logs/status.html"
    if not out.is_absolute():
        out = ROOT / out
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(build_html(), encoding="utf-8")
    print(f"wrote {out} ({out.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
