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

LINK_RE = re.compile(r"(!?\[[^\]]*\]\()([^)\s]+)(\s+\"[^\"]*\")?(\))")


def on_page_markdown(markdown, page, config, files):  # noqa: ARG001
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
