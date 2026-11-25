"""MkDocs hook: rewrite links/images that point outside ``docs/``.

The repository keeps ``README.md``, ``CHANGELOG.md``, ``SECURITY.md`` etc. at the
repository root, while MkDocs only serves files below ``docs/``. Links such as
``../../CHANGELOG.md`` would 404 on the published site, so rewrite exactly those
(and only those) to GitHub URLs. Links that stay inside ``docs/`` are left alone.
"""

from __future__ import annotations

import posixpath
import re
from urllib.parse import quote

GITHUB_BLOB = "https://github.com/HeyTycho/xtervo/blob/main/"
GITHUB_RAW = "https://raw.githubusercontent.com/HeyTycho/xtervo/main/"

ZH_SITE = "https://heytycho.github.io/xtervo/"
EN_SITE = "https://heytycho.github.io/xtervo/en/"

# Chinese source page -> path under the English site. Keep in sync with docs/en/
# and mkdocs.en.yml. Pages not listed fall back to the English home in the header.
ZH_TO_EN = {
    "README.md": "",
    "terminal-behavior.md": "terminal-behavior/",
    "terminal-menu-find.md": "terminal-menu-find/",
    "terminal-ui-basics.md": "terminal-ui-basics/",
    "themes.md": "themes/",
    "SSH_SFTP.md": "ssh-sftp/",
    "VAULT.md": "vault/",
    "PLUGIN_INSTALL.md": "plugins/",
    "PLUGIN_V3.md": "plugin-api-v3/",
    "MCP.md": "mcp/",
    "DIAGNOSTICS.md": "diagnostics/",
    "LOGGING.md": "logging/",
}

# English source page -> path under the Chinese site. Empty means the Chinese home.
EN_TO_ZH = {
    "index.md": "",
    "installation.md": "",
    "shortcuts.md": "",
    "known-limitations.md": "",
    "faq.md": "",
    "terminal-behavior.md": "terminal-behavior/",
    "terminal-menu-find.md": "terminal-menu-find/",
    "terminal-ui-basics.md": "terminal-ui-basics/",
    "themes.md": "themes/",
    "ssh-sftp.md": "SSH_SFTP/",
    "vault.md": "VAULT/",
    "plugins.md": "PLUGIN_INSTALL/",
    "plugin-api-v3.md": "PLUGIN_V3/",
    "mcp.md": "MCP/",
    "diagnostics.md": "DIAGNOSTICS/",
    "logging.md": "LOGGING/",
}

LINK_RE = re.compile(r"(!?\[[^\]]*\]\()([^)\s]+)(\s+\"[^\"]*\")?(\))")


def _page_url(base: str, src: str) -> str:
    if src in ("index.md", "README.md"):
        return base
    return base + src.removesuffix(".md") + "/"


def _set_alternates(page, config) -> None:
    if page.meta is None:
        page.meta = {}
    if page.meta.get("alternate"):
        return

    src = getattr(page.file, "src_uri", None) or page.file.src_path
    theme = config.get("theme") or {}
    language = str(theme.get("language", "") if hasattr(theme, "get") else getattr(theme, "language", ""))
    if language.startswith("en"):
        zh_path = EN_TO_ZH.get(src, "")
        page.meta["alternate"] = [
            {"name": "简体中文", "link": ZH_SITE + zh_path, "lang": "zh"},
            {"name": "English", "link": _page_url(EN_SITE, src), "lang": "en"},
        ]
    else:
        en_path = ZH_TO_EN.get(src)
        if en_path is None:
            return
        page.meta["alternate"] = [
            {"name": "简体中文", "link": _page_url(ZH_SITE, src), "lang": "zh"},
            {"name": "English", "link": EN_SITE + en_path, "lang": "en"},
        ]


def on_page_markdown(markdown, page, config, files):  # noqa: ARG001
    _set_alternates(page, config)
    src = getattr(page.file, "src_uri", None) or page.file.src_path
    base = posixpath.dirname(src)

    def rewrite(match: re.Match) -> str:
        prefix, target = match.group(1), match.group(2)
        if target.startswith(("http://", "https://", "mailto:", "#", "/")):
            return match.group(0)
        path, _, anchor = target.partition("#")
        resolved = posixpath.normpath(posixpath.join(base, path))
        if not (resolved == ".." or resolved.startswith("../")):
            return match.group(0)

        relative = resolved.lstrip("./")
        url = (GITHUB_RAW if prefix.startswith("!") else GITHUB_BLOB) + quote(
            relative, safe="/"
        )
        if anchor:
            url += "#" + anchor
        return f"{prefix}{url}{match.group(4)}"

    return LINK_RE.sub(rewrite, markdown)
