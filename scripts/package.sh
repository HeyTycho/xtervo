#!/usr/bin/env bash
# 发布打包：构建 release → 复制到输出目录 → strip → 体积门禁。
#
# 开发用的 target/release/rusttabby-app 带 debug=1、保留符号，供 perf/flamegraph；
# 这里只处理复制出来的发布副本，绝不就地 strip 开发二进制。
#
#   ./scripts/package.sh                     # → dist/rusttabby-app
#   ./scripts/package.sh /tmp/dist           # 位置参数指定输出目录
#   RUSTTABBY_DIST=/tmp/dist ./scripts/package.sh
#   SKIP_BUILD=1 ./scripts/package.sh        # 复用已有 release 二进制
#
# 环境变量：
#   RUSTTABBY_DIST  输出目录（默认 dist；位置参数优先）
#   MAX_MB          发布产物体积上限，超过则失败（默认 40）
#   SKIP_SMOKE      1 = 跳过 strip 后的 --help 冒烟（交叉编译时用）
#   STRIP           strip 命令（默认自动探测 strip / llvm-strip）
#   STRIP_FLAGS     strip 参数（默认 --strip-all）
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"

DIST="${1:-${RUSTTABBY_DIST:-dist}}"
BIN=target/release/rusttabby-app
OUT="$DIST/rusttabby-app"
MAX_MB="${MAX_MB:-40}"

die() { echo "!! $*" >&2; exit 1; }
size_of() { wc -c < "$1" | tr -d '[:space:]'; }
human() { awk -v b="$1" 'BEGIN { printf "%.1f MB", b / 1048576 }'; }

usage() {
    cat <<'EOF'
用法: ./scripts/package.sh [输出目录]

  -h, --help     显示本帮助

环境变量:
  RUSTTABBY_DIST 输出目录（默认 dist；位置参数优先）
  MAX_MB         产物体积上限（默认 40）
  SKIP_BUILD=1   复用已有 release 二进制
  SKIP_SMOKE=1   跳过 --help 冒烟（交叉编译时用）
  STRIP          strip 命令（默认探测 strip / llvm-strip）
  STRIP_FLAGS    strip 参数（默认 --strip-all）
EOF
}

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
esac

[[ "$MAX_MB" =~ ^[0-9]+$ ]] || die "MAX_MB 必须是整数：$MAX_MB"

# 防止把发布副本写回开发目录，意外 strip 掉 perf 依赖的符号。
BIN_DIR="$(readlink -f "$(dirname "$BIN")")"
DIST_DIR="$(readlink -m "$DIST")"
[[ "$DIST_DIR" != "$BIN_DIR" ]] || die "输出目录不能是 $BIN_DIR（会覆盖开发二进制）"

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
    echo "== 构建 release（保留符号，供 perf/flamegraph）=="
    cargo build --release -p rusttabby-app --bin rusttabby-app
fi
[[ -x "$BIN" ]] || die "找不到 $BIN；先 cargo build --release -p rusttabby-app"

# strip 工具：$STRIP > strip > llvm-strip
STRIP_BIN="${STRIP:-}"
if [[ -z "$STRIP_BIN" ]]; then
    for c in strip llvm-strip; do
        if command -v "$c" >/dev/null 2>&1; then
            STRIP_BIN="$c"
            break
        fi
    done
fi
[[ -n "$STRIP_BIN" ]] || die "找不到 strip：安装 binutils 或设置 STRIP=/path/to/strip"

mkdir -p "$DIST"
# 先写 $OUT.stripped 再原子替换，失败时不留半成品。
TMP="$OUT.stripped"
trap 'rm -f "$TMP"' EXIT
cp -f "$BIN" "$TMP"

BEFORE="$(size_of "$TMP")"
read -r -a STRIP_ARGS <<<"${STRIP_FLAGS:---strip-all}"
echo "== strip 发布副本（$STRIP_BIN ${STRIP_ARGS[*]}）=="
"$STRIP_BIN" "${STRIP_ARGS[@]}" "$TMP"

# 冒烟：--help 在初始化窗口前返回，不需要 DISPLAY；交叉编译产物无法本机运行，跳过。
if [[ "${SKIP_SMOKE:-0}" != 1 ]]; then
    echo "== 冒烟：rusttabby-app --help =="
    "$TMP" --help >/dev/null || die "strip 后二进制无法运行"
fi

AFTER="$(size_of "$TMP")"
MAX_BYTES=$((MAX_MB * 1024 * 1024))
if ((AFTER > MAX_BYTES)); then
    die "发布产物 $AFTER 字节，超过 ${MAX_MB} MB 门禁"
fi
mv -f "$TMP" "$OUT"

echo
echo "== 体积 =="
printf '  开发二进制  %s（%s，保留符号）\n' "$BIN" "$(human "$BEFORE")"
printf '  发布产物    %s（%s，stripped）\n' "$OUT" "$(human "$AFTER")"
awk -v a="$BEFORE" -v b="$AFTER" 'BEGIN { if (a > 0) printf "  缩减        %.1f%%\n", 100 * (a - b) / a }'
if command -v file >/dev/null 2>&1; then
    printf '  file        %s\n' "$(file -b "$OUT")"
fi
printf '  门禁        ≤ %s MB ✓\n' "$MAX_MB"
echo
echo "发布产物： $OUT"
