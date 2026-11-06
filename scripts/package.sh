#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 发布打包：release 构建 → strip 副本 → tar.gz + .deb + 校验和。
#
#   ./scripts/package.sh                     # → dist/ 全部产物
#   ./scripts/package.sh /tmp/dist           # 位置参数指定输出目录
#   TERVO_DIST=/tmp/dist ./scripts/package.sh
#   SKIP_BUILD=1 ./scripts/package.sh        # 复用已有 release 二进制
#   SKIP_DEB=1 ./scripts/package.sh          # 只出 tar.gz / 二进制
#
# 可复现：同 commit、同工具链、clean worktree 下重复执行，产物字节一致。
# 默认用 HEAD 的提交时间作为 SOURCE_DATE_EPOCH（二进制里的 built 时间、
# tar/deb 内文件 mtime、gzip/deb 时间戳都取它）；也可显式覆盖。
#
# 环境变量：
#   TERVO_DIST      输出目录（默认 dist；位置参数优先）
#   MAX_MB          发布二进制体积上限（默认 64）
#   SKIP_BUILD      1 = 不构建，直接打包现有 release 二进制
#   SKIP_SMOKE      1 = 跳过 strip 后的 --help/--version 冒烟（交叉编译时用）
#   SKIP_TAR        1 = 不生成 tar.gz
#   SKIP_DEB        1 = 不生成 .deb
#   ALLOW_DIRTY     1 = 允许未提交修改（默认拒绝，避免产物 commit 与内容不符）
#   SOURCE_DATE_EPOCH                 固定时间戳（默认 HEAD 提交时间）
#   DEB_VERSION     覆盖 .deb 版本（默认 <cargo version>-1）
#   DEB_DEPENDS     覆盖 .deb Depends（默认 dpkg-shlibdeps + X11/XKB 运行库）
#   STRIP           strip 命令（默认自动探测 strip / llvm-strip）
#   STRIP_FLAGS     strip 参数（默认 --strip-all）
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

export CARGO_HOME="${CARGO_HOME:-$ROOT/.cargo}"
export LC_ALL=C
export TZ=UTC

DIST="${1:-${TERVO_DIST:-dist}}"
BIN=target/release/tervo
MAX_MB="${MAX_MB:-64}"
TARBALL_NAME=""
DEB_NAME=""
STAGE=""
DEBROOT=""

die() { echo "!! $*" >&2; exit 1; }
size_of() { wc -c <"$1" | tr -d '[:space:]'; }
human() { awk -v b="$1" 'BEGIN { printf "%.1f MB", b / 1048576 }'; }

usage() {
    cat <<'EOF'
用法: ./scripts/package.sh [输出目录]

  -h, --help     显示本帮助

环境变量:
  TERVO_DIST      输出目录（默认 dist；位置参数优先）
  MAX_MB          发布二进制体积上限（默认 64）
  SKIP_BUILD=1    复用已有 release 二进制
  SKIP_SMOKE=1    跳过 strip 后的 --help/--version 冒烟（交叉编译时用）
  SKIP_TAR=1      不生成 tar.gz
  SKIP_DEB=1      不生成 .deb
  ALLOW_DIRTY=1   允许未提交修改
  SOURCE_DATE_EPOCH               固定时间戳（默认 HEAD 提交时间）
  DEB_VERSION     覆盖 .deb 版本（默认 <cargo version>-1）
  DEB_DEPENDS     覆盖 .deb Depends
  STRIP / STRIP_FLAGS            strip 命令与参数
EOF
}

cleanup() {
    [[ -z "$STAGE" ]] || rm -rf -- "$(dirname -- "$STAGE")"
    [[ -z "$DEBROOT" ]] || rm -rf -- "$DEBROOT"
    [[ -z "${SHLIBS_DIR:-}" ]] || rm -rf -- "$SHLIBS_DIR"
    [[ -z "${BIN_TMP:-}" ]] || rm -f -- "$BIN_TMP"
    [[ -z "${TAR_TMP:-}" ]] || rm -f -- "$TAR_TMP"
    [[ -z "${DEB_TMP:-}" ]] || rm -f -- "$DEB_TMP"
}
trap cleanup EXIT

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

# ---- 版本 / git 元信息 --------------------------------------------------
VERSION="$(awk '
    /^\[workspace\.package\]/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)"
[[ -n "$VERSION" ]] || die "无法从 Cargo.toml 读取 workspace.package.version"

GIT_COMMIT="${TERVO_GIT_COMMIT:-}"
GIT_DIRTY=0
GIT_COMMIT_TIME=""
if git rev-parse --git-dir >/dev/null 2>&1; then
    [[ -n "$GIT_COMMIT" ]] || GIT_COMMIT="$(git rev-parse --short=12 HEAD)"
    GIT_COMMIT_TIME="$(git show -s --format=%ct HEAD)"
    if [[ -n "$(git status --porcelain --untracked-files=normal 2>/dev/null)" ]]; then
        GIT_DIRTY=1
    fi
else
    GIT_COMMIT="${GIT_COMMIT:-unknown}"
fi
if [[ "$GIT_DIRTY" -eq 1 && "${ALLOW_DIRTY:-0}" != 1 ]]; then
    die "工作区有未提交修改：产物里的 commit 会与内容不符。先提交，或 ALLOW_DIRTY=1 强制打包"
fi
[[ -n "$GIT_COMMIT_TIME" ]] || GIT_COMMIT_TIME="$(date +%s)"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$GIT_COMMIT_TIME}"
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]] || die "SOURCE_DATE_EPOCH 必须是 Unix 秒：$SOURCE_DATE_EPOCH"
export SOURCE_DATE_EPOCH
export TERVO_GIT_COMMIT="$GIT_COMMIT"
export TERVO_GIT_DIRTY="$GIT_DIRTY"

BUILT="$(date -u -d "@$SOURCE_DATE_EPOCH" '+%Y-%m-%dT%H:%M:%SZ')"
TARGET_TRIPLE="$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')"
[[ -n "$TARGET_TRIPLE" ]] || TARGET_TRIPLE="$(uname -m)-unknown-linux-gnu"
ARCH="$(uname -m)"
case "$TARGET_TRIPLE" in
    x86_64-*) DEB_ARCH=amd64 ;;
    aarch64-*) DEB_ARCH=arm64 ;;
    *) DEB_ARCH="$(dpkg --print-architecture 2>/dev/null || echo "$ARCH")" ;;
esac
DEB_VERSION="${DEB_VERSION:-$VERSION-1}"

DIRTY_LABEL=""
if [[ "$GIT_DIRTY" -eq 1 ]]; then
    DIRTY_LABEL=" (dirty)"
fi
echo "== Tervo $VERSION =="
printf '  commit      %s%s\n' "$GIT_COMMIT" "$DIRTY_LABEL"
printf '  built       %s\n' "$BUILT"
printf '  target      %s\n' "$TARGET_TRIPLE"
printf '  source date %s\n' "$SOURCE_DATE_EPOCH"

# ---- 构建 ----------------------------------------------------------------
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
    echo "== 构建 release（保留符号，供 perf/flamegraph）=="
    cargo build --release -p tervo-app --bin tervo
fi
[[ -x "$BIN" ]] || die "找不到 $BIN；先 cargo build --release -p tervo"

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
DIST="$(readlink -m "$DIST")"
BIN_TMP="$DIST/.tervo-stripped.$$"
cp -f "$BIN" "$BIN_TMP"
BEFORE="$(size_of "$BIN")"
read -r -a STRIP_ARGS <<<"${STRIP_FLAGS:---strip-all}"
echo "== strip 发布副本（$STRIP_BIN ${STRIP_ARGS[*]}）=="
"$STRIP_BIN" "${STRIP_ARGS[@]}" "$BIN_TMP"

# 冒烟：--help / --version 都在初始化窗口前返回，不需要 DISPLAY。
if [[ "${SKIP_SMOKE:-0}" != 1 ]]; then
    echo "== 冒烟：--help / --version =="
    "$BIN_TMP" --help >/dev/null || die "strip 后二进制 --help 失败"
    VERSION_OUT="$("$BIN_TMP" --version)" || die "strip 后二进制 --version 失败"
    printf '%s\n' "$VERSION_OUT" | grep -qxF "Tervo $VERSION" || die "--version 版本不是 $VERSION：$VERSION_OUT"
    if [[ "$GIT_DIRTY" -eq 1 ]]; then
        printf '%s\n' "$VERSION_OUT" | grep -qxF "commit:  $GIT_COMMIT (dirty)" \
            || die "--version 未记录 dirty commit $GIT_COMMIT：$VERSION_OUT"
    else
        printf '%s\n' "$VERSION_OUT" | grep -qxF "commit:  $GIT_COMMIT" \
            || die "--version 未记录 commit $GIT_COMMIT：$VERSION_OUT"
    fi
    printf '%s\n' "$VERSION_OUT" | grep -qxF "built:   $BUILT" \
        || die "--version 的 built 时间不是 $BUILT（二进制过期？重跑不带 SKIP_BUILD）：$VERSION_OUT"
fi

AFTER="$(size_of "$BIN_TMP")"
MAX_BYTES=$((MAX_MB * 1024 * 1024))
if ((AFTER > MAX_BYTES)); then
    die "发布产物 $AFTER 字节，超过 ${MAX_MB} MB 门禁"
fi
mv -f "$BIN_TMP" "$DIST/tervo-stripped"
BIN_TMP=""
chmod 755 "$DIST/tervo-stripped"
# 兼容旧的 dist/tervo 路径；校验和只针对 tervo-stripped 本体。
ln -sfn tervo-stripped "$DIST/tervo"
BINARY="$DIST/tervo-stripped"

# ---- tar.gz --------------------------------------------------------------
if [[ "${SKIP_TAR:-0}" != 1 ]]; then
    STAGE_NAME="tervo-$VERSION-linux-$ARCH"
    STAGE_PARENT="$DIST/.stage"
    STAGE="$STAGE_PARENT/$STAGE_NAME"
    rm -rf -- "$STAGE_PARENT"
    mkdir -p -- "$STAGE/share/applications" \
        "$STAGE/share/icons/hicolor/512x512/apps" \
        "$STAGE/share/icons/hicolor/64x64/apps"
    install -m 755 "$BINARY" "$STAGE/tervo"
    install -m 755 scripts/install.sh "$STAGE/install.sh"
    install -m 644 LICENSE "$STAGE/LICENSE"
    install -m 644 README.md "$STAGE/README.md"
    install -m 644 THIRD_PARTY_NOTICES.md "$STAGE/THIRD_PARTY_NOTICES.md"
    install -m 644 docs/LICENSING.md "$STAGE/LICENSING.md"
    install -m 644 packaging/tervo.desktop "$STAGE/share/applications/tervo.desktop"
    install -m 644 assets/icon.png "$STAGE/share/icons/hicolor/512x512/apps/tervo.png"
    install -m 644 assets/icon-small.png "$STAGE/share/icons/hicolor/64x64/apps/tervo.png"
    {
        printf 'format=1\n'
        printf 'version=%s\n' "$VERSION"
        printf 'commit=%s\n' "$GIT_COMMIT"
        printf 'dirty=%s\n' "$GIT_DIRTY"
        printf 'built=%s\n' "$BUILT"
        printf 'target=%s\n' "$TARGET_TRIPLE"
        printf 'source_date_epoch=%s\n' "$SOURCE_DATE_EPOCH"
    } >"$STAGE/BUILD-INFO"
    chmod 644 "$STAGE/BUILD-INFO"
    # 固定 mtime/权限，tar 才能逐字节复现。
    find "$STAGE" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
    touch -h -d "@$SOURCE_DATE_EPOCH" "$STAGE_PARENT"
    chmod 755 "$STAGE_PARENT" "$STAGE" "$STAGE/share" "$STAGE/share/applications" \
        "$STAGE/share/icons" "$STAGE/share/icons/hicolor" \
        "$STAGE/share/icons/hicolor/512x512" "$STAGE/share/icons/hicolor/512x512/apps" \
        "$STAGE/share/icons/hicolor/64x64" "$STAGE/share/icons/hicolor/64x64/apps"
    TARBALL_NAME="$STAGE_NAME.tar.gz"
    TAR_TMP="$DIST/.$TARBALL_NAME.$$"
    echo "== 生成 $TARBALL_NAME =="
    tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
        --format=gnu -C "$STAGE_PARENT" -cf - "$STAGE_NAME" | gzip -n -9 >"$TAR_TMP"
    mv -f "$TAR_TMP" "$DIST/$TARBALL_NAME"
    TAR_TMP=""
fi

# ---- .deb ----------------------------------------------------------------
if [[ "${SKIP_DEB:-0}" != 1 ]]; then
    command -v dpkg-deb >/dev/null 2>&1 || die "找不到 dpkg-deb（安装 dpkg-dev）"
    DEBROOT="$DIST/.deb-root"
    rm -rf -- "$DEBROOT"
    mkdir -p -- "$DEBROOT/DEBIAN" "$DEBROOT/usr/bin" \
        "$DEBROOT/usr/share/applications" \
        "$DEBROOT/usr/share/icons/hicolor/512x512/apps" \
        "$DEBROOT/usr/share/icons/hicolor/64x64/apps" \
        "$DEBROOT/usr/share/doc/tervo"
    install -m 755 "$BINARY" "$DEBROOT/usr/bin/tervo"
    install -m 644 packaging/tervo.desktop "$DEBROOT/usr/share/applications/tervo.desktop"
    install -m 644 assets/icon.png "$DEBROOT/usr/share/icons/hicolor/512x512/apps/tervo.png"
    install -m 644 assets/icon-small.png "$DEBROOT/usr/share/icons/hicolor/64x64/apps/tervo.png"
    install -m 644 LICENSE "$DEBROOT/usr/share/doc/tervo/copyright"
    install -m 644 LICENSE "$DEBROOT/usr/share/doc/tervo/LICENSE"
    install -m 644 README.md "$DEBROOT/usr/share/doc/tervo/README.md"
    install -m 644 THIRD_PARTY_NOTICES.md "$DEBROOT/usr/share/doc/tervo/THIRD_PARTY_NOTICES.md"
    install -m 644 docs/LICENSING.md "$DEBROOT/usr/share/doc/tervo/LICENSING.md"
    {
        printf 'format=1\n'
        printf 'version=%s\n' "$VERSION"
        printf 'commit=%s\n' "$GIT_COMMIT"
        printf 'dirty=%s\n' "$GIT_DIRTY"
        printf 'built=%s\n' "$BUILT"
        printf 'target=%s\n' "$TARGET_TRIPLE"
        printf 'source_date_epoch=%s\n' "$SOURCE_DATE_EPOCH"
    } >"$DEBROOT/usr/share/doc/tervo/BUILD-INFO"
    chmod 644 "$DEBROOT/usr/share/doc/tervo/BUILD-INFO"

    # Depends：优先 dpkg-shlibdeps（NEEDED 库），再补 Slint/winit 运行时 dlopen 的库。
    DLOPEN_DEPS="libx11-6, libx11-xcb1, libxkbcommon0, libxkbcommon-x11-0, libxcursor1, libxrandr2, libxi6"
    SHLIB_DEPS=""
    if command -v dpkg-shlibdeps >/dev/null 2>&1; then
        SHLIBS_DIR="$DIST/.deb-shlibdeps"
        mkdir -p -- "$SHLIBS_DIR/debian"
        install -m 755 "$BINARY" "$SHLIBS_DIR/tervo"
        cat >"$SHLIBS_DIR/debian/control" <<'EOF'
Source: tervo
Section: x11
Priority: optional
Maintainer: Tervo maintainers <tervo@localhost>
Standards-Version: 4.6.2

Package: tervo
Architecture: any
Depends: ${shlibs:Depends}, ${misc:Depends}
Description: Tervo terminal emulator
 Native terminal emulator.
EOF
        SHLIB_DEPS="$(
            cd "$SHLIBS_DIR"
            dpkg-shlibdeps -O -e./tervo 2>/dev/null | sed -n 's/^shlibs:Depends=//p'
        )" || true
    fi
    if [[ -n "${DEB_DEPENDS:-}" ]]; then
        DEPENDS="$DEB_DEPENDS"
    elif [[ -n "$SHLIB_DEPS" ]]; then
        DEPENDS="$SHLIB_DEPS, $DLOPEN_DEPS"
    else
        DEPENDS="libc6, libgcc-s1, libstdc++6, libfontconfig1, libfreetype6, $DLOPEN_DEPS"
    fi

    INSTALLED_KB="$(du -sk --apparent-size "$DEBROOT/usr" | awk '{print $1}')"
    DEB_NAME="tervo_${DEB_VERSION}_${DEB_ARCH}.deb"
    cat >"$DEBROOT/DEBIAN/control" <<EOF
Package: tervo
Version: $DEB_VERSION
Architecture: $DEB_ARCH
Maintainer: Tervo maintainers <tervo@localhost>
Installed-Size: $INSTALLED_KB
Section: x11
Priority: optional
Depends: $DEPENDS
Recommends: libgl1, fonts-dejavu-core
Description: Native terminal emulator written in Rust + Slint
 Tervo is a native terminal emulator focused on low memory and input
 latency. It supports tabs, splits, profiles, SSH/SFTP, Sixel images,
 session restore and Tabby configuration import.
 .
 Tervo is an independent open-source project; it is not affiliated with
 Tabby or its authors.
EOF

    # md5sums 必须排序且路径稳定，才能复现。
    (
        cd "$DEBROOT"
        find . -type f ! -path './DEBIAN/*' -printf '%P\0' \
            | sort -z \
            | xargs -0 -r md5sum >DEBIAN/md5sums
    )
    chmod 755 "$DEBROOT/DEBIAN"
    chmod 644 "$DEBROOT/DEBIAN/control" "$DEBROOT/DEBIAN/md5sums"
    find "$DEBROOT" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

    echo "== 生成 $DEB_NAME =="
    DEB_TMP="$DIST/.$DEB_NAME.$$"
    dpkg-deb --build --root-owner-group "$DEBROOT" "$DEB_TMP" >/dev/null
    mv -f "$DEB_TMP" "$DIST/$DEB_NAME"
    DEB_TMP=""
fi

# ---- 校验和 --------------------------------------------------------------
echo "== 校验和 =="
(
    cd "$DIST"
    sha256sum tervo-stripped >tervo-stripped.sha256
    : >SHA256SUMS
    sha256sum tervo-stripped >>SHA256SUMS
    [[ -z "$TARBALL_NAME" ]] || sha256sum "$TARBALL_NAME" >>SHA256SUMS
    [[ -z "$DEB_NAME" ]] || sha256sum "$DEB_NAME" >>SHA256SUMS
    sort -k2 SHA256SUMS -o SHA256SUMS
)

echo
echo "== 产物 =="
printf '  二进制      %s（%s，stripped）\n' "$BINARY" "$(human "$AFTER")"
printf '  开发二进制  %s（%s，保留符号）\n' "$BIN" "$(human "$BEFORE")"
awk -v a="$BEFORE" -v b="$AFTER" 'BEGIN { if (a > 0) printf "  缩减        %.1f%%\n", 100 * (a - b) / a }'
[[ -z "$TARBALL_NAME" ]] || printf '  tar.gz      %s\n' "$DIST/$TARBALL_NAME"
[[ -z "$DEB_NAME" ]] || printf '  deb         %s\n' "$DIST/$DEB_NAME"
printf '  校验和      %s\n' "$DIST/SHA256SUMS"
printf '  门禁        ≤ %s MB ✓\n' "$MAX_MB"
echo
echo "发布产物： $DIST"
