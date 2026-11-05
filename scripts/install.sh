#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 安装 Tervo 到 $PREFIX（默认 ~/.local），附带 .desktop 与图标。
#
#   ./scripts/install.sh                  # 装到 ~/.local（默认安装 strip 后的副本）
#   PREFIX=/tmp/prefix ./scripts/install.sh
#   ./scripts/install.sh --prefix /tmp/prefix
#   ./scripts/install.sh --no-strip       # 保留符号（调试用）
#   PREFIX=/usr/local sudo -E ./scripts/install.sh
#
# 注意：target/release/tervo 是开发二进制（debug=1，保留符号，供
# perf/flamegraph）；安装时先复制到临时目录再 strip，开发二进制保持原样。
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX="${PREFIX:-$HOME/.local}"
DO_STRIP="${TERVO_STRIP:-1}"
case "$DO_STRIP" in
    1 | true | yes | on) DO_STRIP=1 ;;
    *) DO_STRIP=0 ;;
esac

usage() {
    cat <<'EOF'
用法: ./scripts/install.sh [选项]

  --prefix DIR   安装根目录（默认取 $PREFIX，即 ~/.local）
  --no-strip     安装未 strip 的二进制（保留符号，调试用）
  -h, --help     显示本帮助

环境变量:
  PREFIX          安装根目录
  TERVO_STRIP 设为 0 等同于 --no-strip
  STRIP           strip 命令（默认自动探测 strip / llvm-strip）
  STRIP_FLAGS     strip 参数（默认 --strip-all）
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)
            [[ $# -ge 2 ]] || { echo "!! --prefix 需要目录参数" >&2; exit 2; }
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#*=}"
            shift
            ;;
        --no-strip)
            DO_STRIP=0
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "!! 未知参数：$1（-h 查看用法）" >&2
            exit 2
            ;;
    esac
done
[[ -n "$PREFIX" ]] || { echo "!! 安装根目录不能为空" >&2; exit 2; }

human() { awk -v b="$1" 'BEGIN { printf "%.1f MB", b / 1048576 }'; }
size_of() { wc -c < "$1" | tr -d '[:space:]'; }

BIN=target/release/tervo
if [[ ! -x "$BIN" ]]; then
    echo "== 构建 release =="
    CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}" cargo build --release -p tervo-app --bin tervo
fi

# 在临时目录里处理安装副本：开发二进制不动，strip 失败也只影响本次安装。
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/tervo-install.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
STAGED="$STAGE/tervo"
BEFORE="$(size_of "$BIN")"
AFTER="$BEFORE"
install -m755 "$BIN" "$STAGED"

if [[ "$DO_STRIP" == 1 ]]; then
    STRIP_BIN="${STRIP:-}"
    if [[ -z "$STRIP_BIN" ]]; then
        for c in strip llvm-strip; do
            if command -v "$c" >/dev/null 2>&1; then
                STRIP_BIN="$c"
                break
            fi
        done
    fi
    if [[ -z "$STRIP_BIN" ]]; then
        echo "  警告：未找到 strip，安装未 strip 的二进制（$(human "$BEFORE")）" >&2
    else
        read -r -a STRIP_ARGS <<<"${STRIP_FLAGS:---strip-all}"
        echo "== strip 安装副本（$STRIP_BIN ${STRIP_ARGS[*]}）=="
        if "$STRIP_BIN" "${STRIP_ARGS[@]}" "$STAGED"; then
            AFTER="$(size_of "$STAGED")"
        else
            echo "  警告：strip 失败，回退为未 strip 的二进制" >&2
            install -m755 "$BIN" "$STAGED"
            AFTER="$BEFORE"
        fi
    fi
fi

SIZE_NOTE="$(human "$AFTER")"
if ((AFTER < BEFORE)); then
    SIZE_NOTE+="，stripped"
fi

echo "== 安装到 $PREFIX（$SIZE_NOTE）=="
install -Dm755 "$STAGED" "$PREFIX/bin/tervo"
install -Dm644 packaging/tervo.desktop "$PREFIX/share/applications/tervo.desktop"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/512x512/apps/tervo.png"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/256x256/apps/tervo.png"
install -Dm644 LICENSE "$PREFIX/share/doc/tervo/LICENSE"
install -Dm644 THIRD_PARTY_NOTICES.md "$PREFIX/share/doc/tervo/THIRD_PARTY_NOTICES.md"
install -Dm644 docs/LICENSING.md "$PREFIX/share/doc/tervo/LICENSING.md"

update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true

echo
echo "已安装："
echo "  可执行文件  $PREFIX/bin/tervo（$SIZE_NOTE）"
echo "  桌面项      $PREFIX/share/applications/tervo.desktop"
echo "  图标        $PREFIX/share/icons/hicolor/*/apps/tervo.png"
echo "  许可证      $PREFIX/share/doc/tervo/LICENSE"
echo
echo "直接运行： tervo"
echo "（确保 $PREFIX/bin 在 PATH 中）"
