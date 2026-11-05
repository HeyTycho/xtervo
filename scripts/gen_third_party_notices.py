#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""重新生成 THIRD_PARTY_NOTICES.md（运行时依赖 + 许可证原文）。

做三件事：
1. `cargo tree -e normal -p tervo-app` 取实际链接进二进制的 crate；
2. 在本地 `.cargo/registry` 里读取每个 crate 的 license 字段与 LICENSE/NOTICE 文件；
3. 按内容去重后写出 Markdown（每个 crate 一行 + 去重后的许可证文本）。

依赖变化后运行：`python3 scripts/gen_third_party_notices.py`
"""
import hashlib
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OURS = {"tervo-app", "tervo-core"}


def runtime_deps() -> list[tuple[str, str]]:
    out = subprocess.run(
        ["cargo", "tree", "-e", "normal", "-p", "tervo-app", "--prefix", "none"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
        env={**__import__("os").environ, "CARGO_HOME": str(ROOT / ".cargo")},
    ).stdout
    deps = set()
    for line in out.splitlines():
        m = re.match(r"^([A-Za-z0-9_-]+) v(\d+\.\d+\.\d+)", line.strip())
        if m:
            deps.add((m.group(1), m.group(2)))
    return sorted(deps)


def main() -> int:
    dep_set = runtime_deps()
    registries = list((ROOT / ".cargo/registry/src").glob("index.crates.io-*"))
    if not registries:
        print("找不到 .cargo/registry/src，先在仓库内构建一次", file=sys.stderr)
        return 1
    registry = registries[0]

    texts: dict[str, tuple[str, str]] = {}
    text_ids: dict[str, str] = {}
    rows = []

    for name, ver in dep_set:
        if name in OURS:
            continue
        crate = registry / f"{name}-{ver}"
        manifest = crate / "Cargo.toml"
        if not manifest.exists():
            continue
        manifest_text = manifest.read_text(errors="ignore")
        m = re.search(r'^license\s*=\s*"([^"]+)"', manifest_text, re.M)
        lic = m.group(1) if m else ""
        files = sorted(
            p
            for p in crate.iterdir()
            if p.is_file() and re.match(r"(LICEN[CS]E|COPYING|NOTICE)", p.name, re.I)
        )
        ids = []
        for f in files:
            try:
                content = f.read_text(errors="replace").strip()
            except OSError:
                continue
            h = hashlib.sha1(content.encode()).hexdigest()[:10]
            if h not in texts:
                texts[h] = (f"{name}-{f.name}", content)
                text_ids[h] = f"L{len(texts)}"
            ids.append(text_ids[h])
        rows.append((name, ver, lic or "see texts", ids))

    lines = [
        "# Third-Party Notices",
        "",
        "Tervo is licensed under **GPL-3.0-only** (see `LICENSE`). This file lists the",
        "third-party Rust crates linked into the Tervo binary and reproduces the license",
        "texts shipped by those crates. It is generated from `cargo tree -e normal`",
        "plus the local cargo registry; regenerate after dependency changes.",
        "",
        "## Why GPL-3.0-only?",
        "",
        "Tervo's UI uses **Slint 1.18**, offered under",
        "`GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`.",
        "This project uses the GPL-3.0 option, so the combined work is GPL-3.0-only.",
        "If you need a permissive/closed-source license, obtain a commercial or",
        "royalty-free license from Slint GmbH; this repository cannot grant one.",
        "",
        "All other dependencies are permissive (MIT / Apache-2.0 / Zlib / BSD) and",
        "compatible with GPL-3.0. Apache-2.0 components remain available under Apache-2.0.",
        "",
        "## Built-in color schemes",
        "",
        "The built-in theme list contains community color palettes (e.g. Tokyo Night,",
        "Dracula, Solarized, Yaru). Color scheme names may be trademarks of their",
        "respective authors; Tervo is not affiliated with them. Only color values are",
        "included, no code or artwork from those projects.",
        "",
        "## Dependencies",
        "",
        "| Crate | Version | License | License texts |",
        "|---|---|---|---|",
    ]
    for name, ver, lic, ids in rows:
        lines.append(f"| `{name}` | {ver} | {lic} | {', '.join(ids) if ids else '—'} |")
    lines.append("")
    lines.append("## License texts")
    lines.append("")
    for h, (label, content) in texts.items():
        lines.append(f"### {text_ids[h]} — {label}")
        lines.append("")
        lines.append("```")
        lines.append(content)
        lines.append("```")
        lines.append("")

    (ROOT / "THIRD_PARTY_NOTICES.md").write_text("\n".join(lines) + "\n")
    print(f"wrote THIRD_PARTY_NOTICES.md: {len(rows)} crates, {len(texts)} unique texts")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
