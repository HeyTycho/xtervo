#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# W-05 打包/安装/升级/回滚/卸载验收。
#
# 覆盖：
#   - package.sh 生成 xtervo-stripped/tar.gz/.deb/校验和；两次打包逐字节一致
#   - tar 安装到干净临时 prefix；桌面项/图标/文档/清单
#   - .deb 用 dpkg-deb -x 解包（不需要 root），校验 Depends/desktop/icon
#   - Xvfb :141 启动 smoke（--exit-after-first；找不到 Xvfb 时跳过并提示）
#   - 配置/Profile v0/v1 → 当前版本迁移，迁移备份保留
#   - 升级保留上一版（$PREFIX/lib/xtervo/previous/xtervo）与历史归档
#   - 回滚来回切换，配置/Profile 校验和不变
#   - 运行中的旧二进制在原子 rename 后仍存活
#   - 卸载幂等；卸载后配置/Profile 仍在
#
# 用法：
#   ./scripts/test_packaging.sh                 # 临时目录，结束即清理
#   KEEP_TEST=1 ./scripts/test_packaging.sh     # 保留临时目录供排查
#   SKIP_BUILD=1 ...                            # 复用 target/release/xtervo
#   SKIP_REPRO=1 ...                            # 跳过二次打包字节对比
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD

WORK="$(mktemp -d "${TMPDIR:-/tmp}/xtervo-packaging-test.XXXXXX")"
XVFB_PID=""
APP_PID=""
CHECKS=0
ALLOW_DIRTY="${ALLOW_DIRTY:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"
SKIP_SMOKE="${SKIP_SMOKE:-0}"

pass() {
    CHECKS=$((CHECKS + 1))
    printf '  PASS  %s\n' "$1"
}
fail() {
    FAILS=$((FAILS + 1))
    printf '  FAIL  %s\n' "$1" >&2
    exit 1
}
note() { printf '\n== %s ==\n' "$1"; }

assert_file() { [[ -f "$1" ]] && pass "$2" || fail "$2（缺文件 $1）"; }
assert_exec() { [[ -x "$1" ]] && pass "$2" || fail "$2（不可执行 $1）"; }
assert_no_file() { [[ ! -e "$1" ]] && pass "$2" || fail "$2（仍存在 $1）"; }
assert_contains() { grep -qxF -- "$2" "$1" >/dev/null 2>&1 && pass "$3" || fail "$3（$1 不含 $2）"; }

cleanup() {
    if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
    fi
    if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
    if [[ "${KEEP_TEST:-0}" == 1 ]]; then
        echo "保留测试目录：$WORK" >&2
    else
        rm -rf -- "$WORK"
    fi
}
trap cleanup EXIT

# ---- Xvfb ---------------------------------------------------------------
DISPLAY_NUM="${PACKAGING_TEST_DISPLAY:-:141}"
XVFB=""
for candidate in \
    "${E2E_XVFB_BIN:-}" \
    "$ROOT/.perf/xvfb/usr/bin/Xvfb" \
    "$ROOT/../../.perf/xvfb/usr/bin/Xvfb" \
    "$(command -v Xvfb 2>/dev/null || true)"; do
    if [[ -n "$candidate" && -x "$candidate" ]]; then
        XVFB="$candidate"
        break
    fi
done

start_xvfb() {
    [[ -n "$XVFB" ]] || return 0
    if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
        echo "复用已有 Xvfb $DISPLAY_NUM"
        return 0
    fi
    "$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp \
        >"$WORK/xvfb.log" 2>&1 &
    XVFB_PID=$!
    for _ in $(seq 1 100); do
        if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.05
    done
    fail "Xvfb $DISPLAY_NUM 启动失败（$WORK/xvfb.log）"
}

smoke_gui() { # <binary> <label>
    local bin=$1 label=$2
    if [[ -z "$XVFB" ]]; then
        echo "  SKIP  $label（找不到 Xvfb）"
        return 0
    fi
    start_xvfb
    if timeout 30 env DISPLAY="$DISPLAY_NUM" SLINT_BACKEND=winit-software \
        HOME="$WORK/home" XDG_CONFIG_HOME="$WORK/xdg" XTERVO_LOG_DIR="$WORK/logs" \
        "$bin" --exit-after-first >"$WORK/smoke-$label.log" 2>&1; then
        pass "$label 启动 smoke（--exit-after-first）"
    else
        cat "$WORK/smoke-$label.log" >&2 || true
        fail "$label 启动 smoke 失败"
    fi
}

# ---- 1. 打包 -------------------------------------------------------------
note "1/8 打包 tar.gz + .deb + 校验和"
DIST="$WORK/dist"
ALLOW_DIRTY="$ALLOW_DIRTY" SKIP_BUILD="$SKIP_BUILD" SKIP_SMOKE="$SKIP_SMOKE" \
    XTERVO_DIST="$DIST" "$ROOT/scripts/package.sh"
BINARY="$DIST/xtervo-stripped"
TARBALL="$(find "$DIST" -maxdepth 1 -name 'xtervo-*.tar.gz' | head -n 1)"
DEB="$(find "$DIST" -maxdepth 1 -name 'xtervo_*.deb' | head -n 1)"
[[ -n "$TARBALL" ]] || fail "找不到 tar.gz 产物"
[[ -n "$DEB" ]] || fail "找不到 .deb 产物"
assert_exec "$BINARY" "xtervo-stripped 存在且可执行"
assert_file "$DIST/xtervo-stripped.sha256" "二进制 SHA-256 文件存在"
assert_file "$DIST/SHA256SUMS" "SHA256SUMS 存在"
(
    cd "$DIST"
    sha256sum -c xtervo-stripped.sha256 >/dev/null
    sha256sum -c SHA256SUMS >/dev/null
) && pass "校验和自检通过" || fail "校验和自检失败"
# 两次打包逐字节一致（同 commit/同工具链；第二次复用二进制）
if [[ "${SKIP_REPRO:-0}" != 1 ]]; then
    note "2/8 可复现性：二次打包并逐字节比较"
    DIST2="$WORK/dist-repro"
    ALLOW_DIRTY="$ALLOW_DIRTY" SKIP_BUILD=1 XTERVO_DIST="$DIST2" "$ROOT/scripts/package.sh" >/dev/null
    cmp -s "$BINARY" "$DIST2/xtervo-stripped" && pass "xtervo-stripped 字节一致" || fail "xtervo-stripped 不一致"
    TARBALL2="$(find "$DIST2" -maxdepth 1 -name 'xtervo-*.tar.gz' | head -n 1)"
    DEB2="$(find "$DIST2" -maxdepth 1 -name 'xtervo_*.deb' | head -n 1)"
    cmp -s "$TARBALL" "$TARBALL2" && pass "tar.gz 字节一致" || fail "tar.gz 不一致"
    cmp -s "$DEB" "$DEB2" && pass ".deb 字节一致" || fail ".deb 不一致"
    diff -u "$DIST/SHA256SUMS" "$DIST2/SHA256SUMS" >/dev/null \
        && pass "SHA256SUMS 一致" || fail "SHA256SUMS 不一致"
else
    note "2/8 可复现性：SKIP_REPRO=1，跳过"
fi

# ---- 3. tar 安装 ---------------------------------------------------------
note "3/8 tar 安装到干净 prefix"
mkdir -p "$WORK/tar"
tar -xzf "$TARBALL" -C "$WORK/tar"
PAYLOAD="$(find "$WORK/tar" -maxdepth 1 -mindepth 1 -type d | head -n 1)"
[[ -n "$PAYLOAD" ]] || fail "tar 解压后没有顶层目录"
for f in xtervo install.sh LICENSE README.md THIRD_PARTY_NOTICES.md LICENSING.md \
    share/applications/xtervo.desktop \
    share/icons/hicolor/512x512/apps/xtervo.png \
    share/icons/hicolor/64x64/apps/xtervo.png; do
    assert_file "$PAYLOAD/$f" "tar 包含 $f"
done

PREFIX="$WORK/prefix"
"$PAYLOAD/install.sh" --prefix "$PREFIX" >"$WORK/install.log" 2>&1 || {
    cat "$WORK/install.log" >&2
    fail "tar 安装失败"
}
assert_exec "$PREFIX/bin/xtervo" "安装后 bin/xtervo 可执行"
assert_file "$PREFIX/share/applications/xtervo.desktop" "桌面项已安装"
assert_file "$PREFIX/share/icons/hicolor/512x512/apps/xtervo.png" "512 图标已安装"
assert_file "$PREFIX/share/icons/hicolor/64x64/apps/xtervo.png" "64 图标已安装"
assert_file "$PREFIX/share/doc/xtervo/LICENSE" "LICENSE 已安装"
assert_file "$PREFIX/share/doc/xtervo/BUILD-INFO" "BUILD-INFO 已安装"
assert_file "$PREFIX/lib/xtervo/manifest" "安装清单已生成"
assert_file "$PREFIX/lib/xtervo/install.sh" "卸载/回滚脚本已安装"
INSTALLED_VERSION="$("$PREFIX/bin/xtervo" --version | sed -n '1s/^xtervo //p')"
[[ -n "$INSTALLED_VERSION" ]] && pass "安装后 --version 可用（$INSTALLED_VERSION）" || fail "--version 无输出"
"$PREFIX/bin/xtervo" --help >/dev/null && pass "安装后 --help 可用" || fail "--help 失败"
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$PREFIX/share/applications/xtervo.desktop" >/dev/null 2>&1 \
        && pass "desktop 文件通过 desktop-file-validate" || fail "desktop 文件校验失败"
fi
# 用户级 prefix 不在桌面会话 PATH 里时，GNOME 会因 TryExec=xtervo 隐藏应用；
# 安装器必须写成本 prefix 的绝对路径（Exec/TryExec/--window action 都要）。
grep -qxF "Exec=$PREFIX/bin/xtervo" "$PREFIX/share/applications/xtervo.desktop" \
    && pass "desktop Exec 为绝对路径" || fail "desktop Exec 不是绝对路径"
grep -qxF "TryExec=$PREFIX/bin/xtervo" "$PREFIX/share/applications/xtervo.desktop" \
    && pass "desktop TryExec 为绝对路径" || fail "desktop TryExec 不是绝对路径"
grep -qxF "Exec=$PREFIX/bin/xtervo --window" "$PREFIX/share/applications/xtervo.desktop" \
    && pass "desktop new-window action 为绝对路径" || fail "desktop new-window action 不是绝对路径"
smoke_gui "$PREFIX/bin/xtervo" "tar-installed"

# ---- 4. 配置/Profile 迁移 ------------------------------------------------
note "4/8 配置/Profile 迁移与备份"
XDG="$WORK/xdg"
mkdir -p "$XDG/xtervo" "$WORK/home"
CONFIG="$XDG/xtervo/config.toml"
PROFILES="$XDG/xtervo/profiles.toml"
cat >"$CONFIG" <<'EOF'
theme = "nord"
font_size = 13.0
EOF
cat >"$PROFILES" <<'EOF'
version = 1
next_id = 2

[[profiles]]
id = 1
name = "old"
shell = "/bin/sh"
args = []
cwd = "/tmp"
env = []
EOF
printf '{}\n' >"$WORK/tabby-empty.yaml"
env HOME="$WORK/home" XDG_CONFIG_HOME="$XDG" XTERVO_LOG_DIR="$WORK/logs" \
    "$PREFIX/bin/xtervo" --import-tabby "$WORK/tabby-empty.yaml" >"$WORK/migrate.log" 2>&1 || {
    cat "$WORK/migrate.log" >&2
    fail "安装后的二进制迁移配置失败"
}
assert_file "$XDG/xtervo/config.toml.v0.bak" "config v0 迁移前备份存在"
assert_file "$XDG/xtervo/profiles.toml.v1.bak" "profiles v1 迁移前备份存在"
assert_contains "$CONFIG" "version = 1" "config 已迁移到 v1"
assert_contains "$CONFIG" "theme = \"nord\"" "config 旧值保留"
assert_contains "$PROFILES" "version = 2" "profiles 已迁移到 v2"
assert_contains "$PROFILES" "kind = \"local\"" "profiles v1 补上 kind"
grep -q 'name = "old"' "$PROFILES" && pass "Profile 旧数据保留" || fail "Profile 丢失"
CFG_SUM="$(sha256sum "$CONFIG" "$PROFILES")"
BACKUP_SUM="$(sha256sum "$XDG/xtervo/config.toml.v0.bak" "$XDG/xtervo/profiles.toml.v1.bak")"

# ---- 5. 运行中升级：原子 rename + 上一版保留 -----------------------------
note "5/8 运行中升级 + 上一版保留"
if [[ -z "$XVFB" ]]; then
    echo "  SKIP  运行中升级（找不到 Xvfb）"
else
    start_xvfb
    env DISPLAY="$DISPLAY_NUM" SLINT_BACKEND=winit-software \
        HOME="$WORK/home" XDG_CONFIG_HOME="$XDG" XTERVO_LOG_DIR="$WORK/logs" \
        "$PREFIX/bin/xtervo" >"$WORK/running.log" 2>&1 &
    APP_PID=$!
    sleep 2
    if ! kill -0 "$APP_PID" 2>/dev/null; then
        cat "$WORK/running.log" >&2 || true
        fail "安装后的二进制无法常驻运行"
    fi
    pass "旧进程运行中（pid $APP_PID）"
fi

# 构造“下一版”发布包：wrapper 报 9.9.9，其余参数转给真实二进制。
NEXT="$WORK/next"
cp -a "$PAYLOAD" "$NEXT"
REAL_BIN="$WORK/real-xtervo"
cp -f "$PREFIX/bin/xtervo" "$REAL_BIN"
chmod 755 "$REAL_BIN"
cat >"$NEXT/xtervo" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
    --version)
        printf 'xtervo 9.9.9\\ncommit:  deadbeefcafe\\nbuilt:   2030-01-01T00:00:00Z\\ntarget:  x86_64-unknown-linux-gnu\\n'
        exit 0
        ;;
    --help)
        echo "fake next version"
        exit 0
        ;;
esac
exec "$REAL_BIN" "\$@"
EOF
chmod 755 "$NEXT/xtervo"
cat >"$NEXT/BUILD-INFO" <<'EOF'
format=1
version=9.9.9
commit=deadbeefcafe
built=2030-01-01T00:00:00Z
target=x86_64-unknown-linux-gnu
EOF
"$NEXT/install.sh" --prefix "$PREFIX" >"$WORK/upgrade.log" 2>&1 || {
    cat "$WORK/upgrade.log" >&2
    fail "升级安装失败"
}
assert_file "$PREFIX/lib/xtervo/previous/xtervo" "上一版保留在 previous/xtervo"
assert_file "$PREFIX/lib/xtervo/previous/BUILD-INFO" "上一版 BUILD-INFO 保留"
NEXT_VERSION_OUT="$("$PREFIX/bin/xtervo" --version)"
grep -q "xtervo 9.9.9" <<<"$NEXT_VERSION_OUT" && pass "升级后 --version 为新版" || fail "升级后版本不对"
if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    pass "原子 rename 后运行中的旧进程仍存活"
else
    if [[ -n "$APP_PID" ]]; then
        fail "升级覆盖后旧进程被杀（应保持运行）"
    fi
fi
[[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
if [[ -n "$APP_PID" ]]; then
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
fi
[[ "$(sha256sum "$CONFIG" "$PROFILES")" == "$CFG_SUM" ]] \
    && pass "升级后配置/Profile 校验和不变" || fail "升级改动了配置/Profile"
VERSIONS_DIR="$PREFIX/lib/xtervo/versions"
find "$VERSIONS_DIR" -mindepth 2 -maxdepth 2 -name xtervo | grep -q . \
    && pass "历史版本归档存在（$VERSIONS_DIR）" || fail "没有历史版本归档"

# ---- 6. 回滚 -------------------------------------------------------------
note "6/8 回滚（可反复切换）"
"$PREFIX/lib/xtervo/install.sh" --rollback --prefix "$PREFIX" >"$WORK/rollback.log" 2>&1 || {
    cat "$WORK/rollback.log" >&2
    fail "回滚失败"
}
"$PREFIX/bin/xtervo" --version | grep -qxF "xtervo $INSTALLED_VERSION" \
    && pass "回滚后 --version 为原版本（$INSTALLED_VERSION）" || fail "回滚后版本不对"
assert_exec "$PREFIX/lib/xtervo/previous/xtervo" "回滚后上一版（9.9.9）保留"
[[ "$(sha256sum "$CONFIG" "$PROFILES")" == "$CFG_SUM" ]] \
    && pass "回滚后配置/Profile 校验和不变" || fail "回滚改动了配置/Profile"
[[ "$(sha256sum "$XDG/xtervo/config.toml.v0.bak" "$XDG/xtervo/profiles.toml.v1.bak")" == "$BACKUP_SUM" ]] \
    && pass "回滚后迁移备份不变" || fail "回滚改动了迁移备份"
smoke_gui "$PREFIX/bin/xtervo" "rollback-installed"
"$PREFIX/lib/xtervo/install.sh" --rollback --prefix "$PREFIX" >/dev/null 2>&1
TOGGLED_VERSION_OUT="$("$PREFIX/bin/xtervo" --version)"
grep -q "xtervo 9.9.9" <<<"$TOGGLED_VERSION_OUT" \
    && pass "再次回滚可切回新版" || fail "二次回滚失败"

# ---- 7. .deb 解包 --------------------------------------------------------
note "7/8 .deb 解包（dpkg-deb -x，不需要 root）"
DEBROOT="$WORK/deb-root"
mkdir -p "$DEBROOT"
dpkg-deb -x "$DEB" "$DEBROOT"
assert_exec "$DEBROOT/usr/bin/xtervo" ".deb 解包后 usr/bin/xtervo 可执行"
assert_file "$DEBROOT/usr/share/applications/xtervo.desktop" ".deb 含 desktop"
assert_file "$DEBROOT/usr/share/icons/hicolor/512x512/apps/xtervo.png" ".deb 含 512 图标"
assert_file "$DEBROOT/usr/share/icons/hicolor/64x64/apps/xtervo.png" ".deb 含 64 图标"
assert_file "$DEBROOT/usr/share/doc/xtervo/copyright" ".deb 含 copyright"
[[ "$(dpkg-deb -f "$DEB" Package)" == "xtervo" ]] && pass ".deb Package=xtervo" || fail ".deb Package 不对"
DEB_ARCH_FIELD="$(dpkg-deb -f "$DEB" Architecture)"
[[ -n "$DEB_ARCH_FIELD" ]] && pass ".deb Architecture=$DEB_ARCH_FIELD" || fail ".deb 缺 Architecture"
DEB_DEPENDS_FIELD="$(dpkg-deb -f "$DEB" Depends)"
[[ -n "$DEB_DEPENDS_FIELD" ]] && pass ".deb 含 Depends" || fail ".deb 缺 Depends"
grep -q "libx11-6" <<<"$DEB_DEPENDS_FIELD" && pass ".deb Depends 含 X11 运行库" || fail "Depends 缺 libx11-6"
"$DEBROOT/usr/bin/xtervo" --version >/dev/null && pass "解包二进制 --version 可用" || fail "解包二进制不可用"
"$DEBROOT/usr/bin/xtervo" --help >/dev/null && pass "解包二进制 --help 可用" || fail "解包二进制 --help 失败"
dpkg-deb -c "$DEB" | grep -q "./usr/share/icons/hicolor/512x512/apps/xtervo.png" \
    && pass ".deb 内容清单含图标" || fail ".deb 内容清单缺图标"

# ---- 8. 卸载 -------------------------------------------------------------
note "8/8 卸载（幂等）与配置保留"
"$PAYLOAD/install.sh" --uninstall --prefix "$PREFIX" >"$WORK/uninstall.log" 2>&1 || {
    cat "$WORK/uninstall.log" >&2
    fail "卸载失败"
}
assert_no_file "$PREFIX/bin/xtervo" "卸载后 bin/xtervo 已移除"
assert_no_file "$PREFIX/share/applications/xtervo.desktop" "卸载后 desktop 已移除"
assert_no_file "$PREFIX/share/icons/hicolor/512x512/apps/xtervo.png" "卸载后图标已移除"
assert_no_file "$PREFIX/lib/xtervo" "卸载后安装状态目录已移除"
assert_file "$CONFIG" "卸载后 config.toml 保留"
assert_file "$PROFILES" "卸载后 profiles.toml 保留"
[[ "$(sha256sum "$CONFIG" "$PROFILES")" == "$CFG_SUM" ]] \
    && pass "卸载后配置/Profile 校验和不变" || fail "卸载改动了配置/Profile"
[[ "$(sha256sum "$XDG/xtervo/config.toml.v0.bak" "$XDG/xtervo/profiles.toml.v1.bak")" == "$BACKUP_SUM" ]] \
    && pass "卸载后迁移备份保留" || fail "卸载删除了迁移备份"
"$PAYLOAD/install.sh" --uninstall --prefix "$PREFIX" >/dev/null 2>&1 \
    && pass "重复卸载幂等（退出码 0）" || fail "重复卸载失败"

# ---- 脚本语法 / shellcheck 风格 -----------------------------------------
note "脚本语法与 shellcheck 风格"
bash -n "$ROOT/scripts/install.sh" "$ROOT/scripts/package.sh" "$ROOT/scripts/test_packaging.sh" \
    && pass "bash -n 通过"
SHELLCHECK_BIN="${SHELLCHECK:-$(command -v shellcheck 2>/dev/null || true)}"
if [[ -n "$SHELLCHECK_BIN" ]]; then
    "$SHELLCHECK_BIN" -s bash -S warning "$ROOT/scripts/install.sh" "$ROOT/scripts/package.sh" \
        "$ROOT/scripts/test_packaging.sh" && pass "shellcheck 通过（warning 级别）" \
        || fail "shellcheck 失败"
else
    echo "  SKIP  shellcheck 未安装（脚本已通过 bash -n；SHELLCHECK=/path 可指定）"
fi

note "结果"
printf '  %d 项检查通过，0 失败\n' "$CHECKS"
if [[ "${KEEP_TEST:-0}" != 1 ]]; then
    echo "  临时目录由 trap 清理；KEEP_TEST=1 可保留产物/日志"
fi
