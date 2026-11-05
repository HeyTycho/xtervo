#!/usr/bin/env bash
# 安装 Rust Tabby 到 $PREFIX（默认 ~/.local），附带 .desktop 与图标。
#
#   ./scripts/install.sh              # 装到 ~/.local
#   PREFIX=/usr/local sudo -E ./scripts/install.sh
set -euo pipefail
cd "$(dirname "$0")/.."

PREFIX="${PREFIX:-$HOME/.local}"
BIN=target/release/rusttabby-app

if [[ ! -x "$BIN" ]]; then
    echo "== 构建 release =="
    CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}" cargo build --release -p rusttabby-app --bin rusttabby-app
fi

echo "== 安装到 $PREFIX =="
install -Dm755 "$BIN" "$PREFIX/bin/rusttabby-app"
install -Dm644 packaging/rusttabby.desktop "$PREFIX/share/applications/rusttabby.desktop"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/512x512/apps/rusttabby.png"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/256x256/apps/rusttabby.png"

update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true

echo
echo "已安装："
echo "  可执行文件  $PREFIX/bin/rusttabby-app"
echo "  桌面项      $PREFIX/share/applications/rusttabby.desktop"
echo "  图标        $PREFIX/share/icons/hicolor/*/apps/rusttabby.png"
echo
echo "直接运行： rusttabby-app"
echo "（确保 $PREFIX/bin 在 PATH 中）"
