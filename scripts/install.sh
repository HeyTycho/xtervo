#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Tervo 安装器：安装 / 升级 / 回滚 / 卸载。
#
# 两种运行方式：
#   1. 仓库内：./scripts/install.sh [--prefix DIR]，按需构建 target/release/tervo；
#   2. 发布包内：tar 解压后 ./install.sh [--prefix DIR]，直接安装旁边的 tervo。
#
# 升级/回滚约定（明确路径）：
#   - 当前二进制      $PREFIX/bin/tervo
#   - 上一版二进制    $PREFIX/lib/tervo/previous/tervo
#   - 历史版本归档    $PREFIX/lib/tervo/versions/<version>-<commit>/tervo
#   - 安装清单        $PREFIX/lib/tervo/manifest
#   - 卸载/回滚脚本   $PREFIX/lib/tervo/install.sh
# 新二进制先在同一文件系统内写临时文件、strip/冒烟，再 rename 覆盖；
# 运行中的旧进程持有旧 inode，不会被写坏（Linux rename 语义）。
# 卸载/回滚绝不触碰 ~/.config/tervo 下的配置、Profile、主题与日志。
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
CALLER_PWD="$PWD"
PAYLOAD_MODE=0
if [[ -x "$SCRIPT_DIR/tervo" ]]; then
    PAYLOAD_MODE=1
fi
REPO_ROOT=""
if [[ "$PAYLOAD_MODE" -eq 0 && -f "$SCRIPT_DIR/../Cargo.toml" ]]; then
    REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"
    cd "$REPO_ROOT"
fi

PREFIX="${PREFIX:-$HOME/.local}"
DO_STRIP="${TERVO_STRIP:-1}"
case "$DO_STRIP" in
    1 | true | yes | on) DO_STRIP=1 ;;
    *) DO_STRIP=0 ;;
esac

usage() {
    cat <<'EOF'
用法: install.sh [选项]

  --prefix DIR   安装根目录（默认 $PREFIX，即 ~/.local）
  --no-strip     安装未 strip 的二进制（调试用；发布包已 strip，忽略）
  --rollback     用上一版替换当前版（当前版会留在上一版路径，可再次回滚）
  --uninstall    卸载安装的文件（保留 ~/.config/tervo 下的配置/Profile）
  -h, --help     显示本帮助

环境变量:
  PREFIX          安装根目录
  TERVO_STRIP     设为 0 等同于 --no-strip
  TERVO_BIN       指定待安装二进制（测试用；默认用发布包/仓库产物）
  SKIP_SMOKE      1 = 跳过安装前冒烟
  STRIP           strip 命令（默认自动探测 strip / llvm-strip）
  STRIP_FLAGS     strip 参数（默认 --strip-all）
EOF
}

die() { echo "!! $*" >&2; exit 1; }
warn() { echo "  警告：$*" >&2; }

MODE=install
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
        --rollback)
            MODE=rollback
            shift
            ;;
        --uninstall)
            MODE=uninstall
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
[[ -n "$PREFIX" ]] || die "安装根目录不能为空"
# 相对 prefix 按调用者当前目录解析；脚本内部会 cd 到仓库根，不能直接用 $PWD。
[[ "$PREFIX" == /* ]] || PREFIX="$CALLER_PWD/$PREFIX"
PREFIX="$(readlink -m -- "$PREFIX")"
[[ "$PREFIX" != "/" ]] || die "拒绝以 / 作为 prefix；请用 --prefix /usr 或默认 ~/.local"

BIN_DIR="$PREFIX/bin"
CUR="$BIN_DIR/tervo"
STATE_DIR="$PREFIX/lib/tervo"
PREV_DIR="$STATE_DIR/previous"
VERSIONS_DIR="$STATE_DIR/versions"
MANIFEST="$STATE_DIR/manifest"
MANAGER="$STATE_DIR/install.sh"
DOC_DIR="$PREFIX/share/doc/tervo"
DESKTOP="$PREFIX/share/applications/tervo.desktop"
ICON512="$PREFIX/share/icons/hicolor/512x512/apps/tervo.png"
ICON64="$PREFIX/share/icons/hicolor/64x64/apps/tervo.png"

TMP_FILES=()
cleanup() {
    if [[ ${#TMP_FILES[@]} -gt 0 ]]; then
        rm -f -- "${TMP_FILES[@]}"
    fi
}
trap cleanup EXIT

make_tmp() { # <dir> <prefix> -> path
    mktemp -- "$1/.$2.XXXXXX"
}

atomic_install() { # <src> <mode> <dest>
    local src=$1 mode=$2 dest=$3 dir tmp
    dir="$(dirname -- "$dest")"
    mkdir -p -- "$dir"
    tmp="$(make_tmp "$dir" "$(basename -- "$dest")")"
    TMP_FILES+=("$tmp")
    if ! install -m "$mode" -- "$src" "$tmp"; then
        rm -f -- "$tmp"
        return 1
    fi
    mv -f -- "$tmp" "$dest"
}

manifest_value() { # <key>
    [[ -f "$MANIFEST" ]] || return 0
    sed -n "s/^$1=//p" "$MANIFEST" | head -n 1
}

version_tag() { # <version> <commit>
    local version=${1:-unknown} commit=${2:-unknown}
    printf '%s-%s' "$version" "$commit" | tr -c 'A-Za-z0-9._+-' '_' | cut -c1-120
}

# 探测二进制的 --version；新版本会在 UI/日志初始化前返回。
# 老版本不认 --version 时由 timeout 兜底，不把它当成安装失败。
probe_binary() { # <path> -> PROBE_VERSION/COMMIT/BUILT/TARGET
    PROBE_VERSION="unknown"
    PROBE_COMMIT="unknown"
    PROBE_BUILT="unknown"
    PROBE_TARGET="unknown"
    [[ -x "$1" ]] || return 0
    local out
    out="$(timeout 5 "$1" --version 2>/dev/null || true)"
    [[ -n "$out" ]] || return 0
    PROBE_VERSION="$(printf '%s\n' "$out" | sed -n '1s/^Tervo  *//p' | awk '{print $1}')"
    PROBE_COMMIT="$(printf '%s\n' "$out" | sed -n 's/^commit:  *//p' | awk '{print $1}')"
    PROBE_BUILT="$(printf '%s\n' "$out" | sed -n 's/^built:  *//p' | head -n 1)"
    PROBE_TARGET="$(printf '%s\n' "$out" | sed -n 's/^target:  *//p' | head -n 1)"
    PROBE_VERSION="${PROBE_VERSION:-unknown}"
    PROBE_COMMIT="${PROBE_COMMIT:-unknown}"
    PROBE_BUILT="${PROBE_BUILT:-unknown}"
    PROBE_TARGET="${PROBE_TARGET:-unknown}"
}

write_buildinfo() { # <dest> <version> <commit> <built> <target> <kind>
    local dest=$1 dir tmp
    dir="$(dirname -- "$dest")"
    mkdir -p -- "$dir"
    tmp="$(make_tmp "$dir" BUILD-INFO)"
    TMP_FILES+=("$tmp")
    {
        printf 'format=1\n'
        printf 'version=%s\n' "$2"
        printf 'commit=%s\n' "$3"
        printf 'built=%s\n' "$4"
        printf 'target=%s\n' "$5"
        printf 'kind=%s\n' "$6"
        printf 'installed_at=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
        printf 'prefix=%s\n' "$PREFIX"
    } >"$tmp"
    mv -f -- "$tmp" "$dest"
}

maybe_strip() { # <path>
    [[ "$DO_STRIP" == 1 ]] || return 0
    # 发布包里的二进制已在打包阶段 strip，重复 strip 没有必要，也避免改变发布校验和。
    [[ "$PAYLOAD_MODE" -eq 0 ]] || return 0
    local strip_bin="${STRIP:-}"
    if [[ -z "$strip_bin" ]]; then
        for candidate in strip llvm-strip; do
            if command -v "$candidate" >/dev/null 2>&1; then
                strip_bin="$candidate"
                break
            fi
        done
    fi
    if [[ -z "$strip_bin" ]]; then
        warn "未找到 strip，安装未 strip 的二进制"
        return 0
    fi
    local -a strip_args=()
    read -r -a strip_args <<<"${STRIP_FLAGS:---strip-all}"
    if ! "$strip_bin" "${strip_args[@]}" "$1"; then
        warn "strip 失败，继续安装未 strip 的副本"
    fi
}

smoke_binary() { # <path>
    if [[ "${SKIP_SMOKE:-0}" == 1 ]]; then
        return 0
    fi
    if timeout 10 "$1" --version >/dev/null 2>&1; then
        return 0
    fi
    if timeout 10 "$1" --help >/dev/null 2>&1; then
        return 0
    fi
    die "$1 冒烟失败（--version/--help 均不可用）"
}

resolve_payload() {
    SRC=""
    PAYLOAD_DIR=""
    if [[ -n "${TERVO_BIN:-}" ]]; then
        local bin_path="$TERVO_BIN"
        [[ "$bin_path" == /* ]] || bin_path="$CALLER_PWD/$bin_path"
        SRC="$(readlink -f -- "$bin_path")"
        [[ -f "$SRC" ]] || die "TERVO_BIN 不存在：$TERVO_BIN"
        PAYLOAD_DIR=""
        return
    fi
    if [[ "$PAYLOAD_MODE" -eq 1 ]]; then
        SRC="$SCRIPT_DIR/tervo"
        PAYLOAD_DIR="$SCRIPT_DIR"
        return
    fi
    [[ -n "$REPO_ROOT" ]] || die "找不到发布包里的 tervo，也不在仓库内；无法安装"
    if [[ ! -x "$REPO_ROOT/target/release/tervo" ]]; then
        echo "== 构建 release =="
        (
            cd "$REPO_ROOT"
            CARGO_HOME="${CARGO_HOME:-$REPO_ROOT/.cargo}" \
                cargo build --release -p tervo-app --bin tervo
        )
    fi
    SRC="$REPO_ROOT/target/release/tervo"
}

install_assets() { # <version> <commit> <built> <target>
    local version=$1 commit=$2 built=$3 target=$4
    local desktop_src license_src readme_src notices_src licensing_src icon512_src icon64_src
    if [[ "$PAYLOAD_MODE" -eq 1 ]]; then
        [[ -n "$PAYLOAD_DIR" ]] || die "内部错误：发布包路径为空"
        desktop_src="$PAYLOAD_DIR/share/applications/tervo.desktop"
        license_src="$PAYLOAD_DIR/LICENSE"
        readme_src="$PAYLOAD_DIR/README.md"
        notices_src="$PAYLOAD_DIR/THIRD_PARTY_NOTICES.md"
        licensing_src="$PAYLOAD_DIR/LICENSING.md"
        icon512_src="$PAYLOAD_DIR/share/icons/hicolor/512x512/apps/tervo.png"
        icon64_src="$PAYLOAD_DIR/share/icons/hicolor/64x64/apps/tervo.png"
    else
        desktop_src="$REPO_ROOT/packaging/tervo.desktop"
        license_src="$REPO_ROOT/LICENSE"
        readme_src="$REPO_ROOT/README.md"
        notices_src="$REPO_ROOT/THIRD_PARTY_NOTICES.md"
        licensing_src="$REPO_ROOT/docs/LICENSING.md"
        icon512_src="$REPO_ROOT/assets/icon.png"
        icon64_src="$REPO_ROOT/assets/icon-small.png"
    fi
    [[ -f "$desktop_src" ]] || die "缺少桌面项：$desktop_src"
    [[ -f "$license_src" ]] || die "缺少 LICENSE：$license_src"
    [[ -f "$readme_src" ]] || die "缺少 README：$readme_src"
    [[ -f "$icon512_src" ]] || die "缺少 512 图标：$icon512_src"
    [[ -f "$icon64_src" ]] || die "缺少 64 图标：$icon64_src"

    # 用户级安装（默认 ~/.local）不一定在 GNOME/桌面会话的 PATH 里：
    # desktop 里写 `TryExec=tervo` 会被 GNOME 当成“命令不存在”而隐藏，
    # Activities 里搜不到。安装时把 Exec/TryExec 固化为本 prefix 的绝对路径；
    # 系统级安装同样适用，且不依赖登录 PATH。
    local desktop_dir desktop_tmp cur_escaped
    desktop_dir="$(dirname -- "$DESKTOP")"
    mkdir -p -- "$desktop_dir"
    desktop_tmp="$(make_tmp "$desktop_dir" tervo.desktop)"
    TMP_FILES+=("$desktop_tmp")
    cur_escaped="${CUR//&/\\&}"
    sed -e "s|^TryExec=tervo$|TryExec=$cur_escaped|" \
        -e "s|^Exec=tervo$|Exec=$cur_escaped|" \
        -e "s|^Exec=tervo --window$|Exec=$cur_escaped --window|" \
        "$desktop_src" >"$desktop_tmp"
    atomic_install "$desktop_tmp" 644 "$DESKTOP"
    atomic_install "$icon512_src" 644 "$ICON512"
    atomic_install "$icon64_src" 644 "$ICON64"
    atomic_install "$license_src" 644 "$DOC_DIR/LICENSE"
    atomic_install "$readme_src" 644 "$DOC_DIR/README.md"
    if [[ -f "$notices_src" ]]; then
        atomic_install "$notices_src" 644 "$DOC_DIR/THIRD_PARTY_NOTICES.md"
    fi
    FILES+=("bin/tervo" "share/applications/tervo.desktop")
    FILES+=("share/icons/hicolor/512x512/apps/tervo.png")
    FILES+=("share/icons/hicolor/64x64/apps/tervo.png")
    FILES+=("share/doc/tervo/LICENSE" "share/doc/tervo/README.md")
    if [[ -f "$DOC_DIR/THIRD_PARTY_NOTICES.md" ]]; then
        FILES+=("share/doc/tervo/THIRD_PARTY_NOTICES.md")
    fi
    if [[ -f "$licensing_src" ]]; then
        atomic_install "$licensing_src" 644 "$DOC_DIR/LICENSING.md"
        FILES+=("share/doc/tervo/LICENSING.md")
    fi
    write_buildinfo "$DOC_DIR/BUILD-INFO" "$version" "$commit" "$built" "$target" install
    FILES+=("share/doc/tervo/BUILD-INFO")
}

archive_current() {
    [[ -e "$CUR" ]] || return 0
    mkdir -p -- "$PREV_DIR" "$VERSIONS_DIR"
    local version commit tag vdir
    if [[ -f "$MANIFEST" ]]; then
        version="$(manifest_value version)"
        commit="$(manifest_value commit)"
    fi
    version="${version:-unknown}"
    commit="${commit:-unknown}"
    tag="$(version_tag "$version" "$commit")"
    vdir="$VERSIONS_DIR/$tag"
    # 同一版本重复安装不覆盖已有归档，保留第一次安装的完整副本。
    if [[ ! -f "$vdir/tervo" ]]; then
        atomic_install "$CUR" 755 "$vdir/tervo"
        if [[ -f "$DOC_DIR/BUILD-INFO" ]]; then
            atomic_install "$DOC_DIR/BUILD-INFO" 644 "$vdir/BUILD-INFO"
        fi
    fi
    # 稳定路径：始终是“当前版被替换前的那一版”。
    atomic_install "$CUR" 755 "$PREV_DIR/tervo"
    if [[ -f "$DOC_DIR/BUILD-INFO" ]]; then
        atomic_install "$DOC_DIR/BUILD-INFO" 644 "$PREV_DIR/BUILD-INFO"
    else
        write_buildinfo "$PREV_DIR/BUILD-INFO" "$version" "$commit" unknown unknown previous
    fi
    printf '%s\n' "$version" >"$STATE_DIR/previous/VERSION.tmp"
    mv -f -- "$STATE_DIR/previous/VERSION.tmp" "$STATE_DIR/previous/VERSION"
}

write_manifest() { # <version> <commit> <built> <target>
    local tmp
    mkdir -p -- "$STATE_DIR"
    tmp="$(make_tmp "$STATE_DIR" manifest)"
    TMP_FILES+=("$tmp")
    {
        printf 'format=1\n'
        printf 'version=%s\n' "$1"
        printf 'commit=%s\n' "$2"
        printf 'built=%s\n' "$3"
        printf 'target=%s\n' "$4"
        printf 'prefix=%s\n' "$PREFIX"
        printf 'files:\n'
        printf '%s\n' "${FILES[@]}"
    } >"$tmp"
    mv -f -- "$tmp" "$MANIFEST"
}

update_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

do_install() {
    local FILES=()
    resolve_payload
    [[ -x "$SRC" ]] || die "待安装二进制不可执行：$SRC"
    mkdir -p -- "$BIN_DIR"

    local staged
    staged="$(make_tmp "$BIN_DIR" tervo.new)"
    TMP_FILES+=("$staged")
    install -m 755 -- "$SRC" "$staged"
    maybe_strip "$staged"
    smoke_binary "$staged"

    local version commit built target
    probe_binary "$staged"
    version="$PROBE_VERSION"
    commit="$PROBE_COMMIT"
    built="$PROBE_BUILT"
    target="$PROBE_TARGET"
    if [[ "$version" == "unknown" && "$PAYLOAD_MODE" -eq 1 && -f "$PAYLOAD_DIR/BUILD-INFO" ]]; then
        version="$(sed -n 's/^version=//p' "$PAYLOAD_DIR/BUILD-INFO" | head -n 1)"
        commit="$(sed -n 's/^commit=//p' "$PAYLOAD_DIR/BUILD-INFO" | head -n 1)"
        built="$(sed -n 's/^built=//p' "$PAYLOAD_DIR/BUILD-INFO" | head -n 1)"
        target="$(sed -n 's/^target=//p' "$PAYLOAD_DIR/BUILD-INFO" | head -n 1)"
    fi
    version="${version:-unknown}"
    commit="${commit:-unknown}"

    echo "== 安装 Tervo $version（commit ${commit}）到 $PREFIX =="
    # 必须在覆盖 docs/BUILD-INFO 之前归档旧版，否则上一版二进制与元数据会错配。
    archive_current
    install_assets "$version" "$commit" "$built" "$target"
    atomic_install "$SCRIPT_DIR/install.sh" 755 "$MANAGER"
    FILES+=("lib/tervo/install.sh")

    mv -f -- "$staged" "$CUR"

    FILES+=("lib/tervo/manifest")
    write_manifest "$version" "$commit" "$built" "$target"
    update_caches

    local previous_note="（首次安装，暂无上一版）"
    if [[ -f "$PREV_DIR/tervo" ]]; then
        previous_note="$PREV_DIR/tervo"
    fi
    echo
    echo "已安装："
    echo "  版本        $version (commit $commit)"
    echo "  可执行文件  $CUR"
    echo "  桌面项      $DESKTOP"
    echo "  图标        $ICON512"
    echo "  文档/许可证 $DOC_DIR"
    echo "  上一版      $previous_note"
    echo "  版本归档    $VERSIONS_DIR"
    echo "  卸载脚本    $MANAGER"
    echo
    echo "  回滚：      $MANAGER --rollback --prefix $PREFIX"
    echo "  卸载：      $MANAGER --uninstall --prefix $PREFIX"
    echo "  （只移除程序文件；~/.config/tervo 的配置/Profile 保留）"
}

do_rollback() {
    local FILES=()
    [[ -f "$PREV_DIR/tervo" ]] || die "没有上一版：$PREV_DIR/tervo；先安装一次新版本后再回滚"
    mkdir -p -- "$BIN_DIR" "$DOC_DIR"

    local staged
    staged="$(make_tmp "$BIN_DIR" tervo.rollback)"
    TMP_FILES+=("$staged")
    install -m 755 -- "$PREV_DIR/tervo" "$staged"
    smoke_binary "$staged"

    local version commit built target
    probe_binary "$staged"
    version="$PROBE_VERSION"
    commit="$PROBE_COMMIT"
    built="$PROBE_BUILT"
    target="$PROBE_TARGET"
    if [[ "$version" == "unknown" && -f "$PREV_DIR/BUILD-INFO" ]]; then
        version="$(sed -n 's/^version=//p' "$PREV_DIR/BUILD-INFO" | head -n 1)"
        commit="$(sed -n 's/^commit=//p' "$PREV_DIR/BUILD-INFO" | head -n 1)"
        built="$(sed -n 's/^built=//p' "$PREV_DIR/BUILD-INFO" | head -n 1)"
        target="$(sed -n 's/^target=//p' "$PREV_DIR/BUILD-INFO" | head -n 1)"
    fi
    version="${version:-unknown}"
    commit="${commit:-unknown}"

    # 先备份“回滚后的当前版”，让 --rollback 可反复来回切。
    local save_bin="" save_info=""
    if [[ -e "$CUR" ]]; then
        save_bin="$(make_tmp "$STATE_DIR" rollback.current)"
        TMP_FILES+=("$save_bin")
        install -m 755 -- "$CUR" "$save_bin"
        if [[ -f "$DOC_DIR/BUILD-INFO" ]]; then
            save_info="$(make_tmp "$STATE_DIR" rollback.current.info)"
            TMP_FILES+=("$save_info")
            cp -f -- "$DOC_DIR/BUILD-INFO" "$save_info"
        fi
    fi

    echo "== 回滚到 Tervo $version（commit ${commit}）=="
    mv -f -- "$staged" "$CUR"
    if [[ -n "$save_bin" ]]; then
        mv -f -- "$save_bin" "$PREV_DIR/tervo"
        if [[ -n "$save_info" ]]; then
            mv -f -- "$save_info" "$PREV_DIR/BUILD-INFO"
        else
            write_buildinfo "$PREV_DIR/BUILD-INFO" unknown unknown unknown unknown previous
        fi
        # 保留被回滚掉版本的归档（若之前未归档）。
        mkdir -p -- "$VERSIONS_DIR"
        local old_version old_commit old_tag
        old_version="$(sed -n 's/^version=//p' "$PREV_DIR/BUILD-INFO" 2>/dev/null | head -n 1)"
        old_commit="$(sed -n 's/^commit=//p' "$PREV_DIR/BUILD-INFO" 2>/dev/null | head -n 1)"
        old_tag="$(version_tag "${old_version:-unknown}" "${old_commit:-unknown}")"
        if [[ ! -f "$VERSIONS_DIR/$old_tag/tervo" ]]; then
            atomic_install "$PREV_DIR/tervo" 755 "$VERSIONS_DIR/$old_tag/tervo"
            atomic_install "$PREV_DIR/BUILD-INFO" 644 "$VERSIONS_DIR/$old_tag/BUILD-INFO"
        fi
    else
        rm -f -- "$PREV_DIR/tervo" "$PREV_DIR/BUILD-INFO"
    fi

    write_buildinfo "$DOC_DIR/BUILD-INFO" "$version" "$commit" "$built" "$target" rollback
    atomic_install "$SCRIPT_DIR/install.sh" 755 "$MANAGER"
    # 回滚无需重装桌面项/图标；manifest 文件列表沿用已有内容（没有则补默认）。
    if [[ -f "$MANIFEST" ]]; then
        mapfile -t FILES < <(sed -n '/^files:$/,$p' "$MANIFEST" | tail -n +2)
    else
        FILES=("bin/tervo" "share/applications/tervo.desktop")
        FILES+=("share/icons/hicolor/512x512/apps/tervo.png")
        FILES+=("share/icons/hicolor/64x64/apps/tervo.png")
        FILES+=("share/doc/tervo/LICENSE" "share/doc/tervo/README.md")
        FILES+=("share/doc/tervo/BUILD-INFO" "lib/tervo/install.sh")
    fi
    if [[ ${#FILES[@]} -eq 0 ]]; then
        FILES=("bin/tervo")
    fi
    write_manifest "$version" "$commit" "$built" "$target"
    update_caches
    echo
    echo "已回滚： $CUR（$version）"
    echo "上一版： $PREV_DIR/tervo（再执行 --rollback 可切回）"
}

do_uninstall() {
    local removed=0 rel
    if [[ -f "$MANIFEST" ]]; then
        while IFS= read -r rel; do
            [[ -n "$rel" ]] || continue
            case "$rel" in
                /* | *..*)
                    warn "忽略可疑清单路径：$rel"
                    continue
                    ;;
            esac
            if [[ -e "$PREFIX/$rel" || -L "$PREFIX/$rel" ]]; then
                rm -f -- "$PREFIX/$rel"
                removed=1
            fi
        done < <(sed -n '/^files:$/,$p' "$MANIFEST" | tail -n +2)
    else
        for rel in \
            "bin/tervo" \
            "share/applications/tervo.desktop" \
            "share/icons/hicolor/512x512/apps/tervo.png" \
            "share/icons/hicolor/64x64/apps/tervo.png" \
            "share/doc/tervo/LICENSE" \
            "share/doc/tervo/README.md" \
            "share/doc/tervo/THIRD_PARTY_NOTICES.md" \
            "share/doc/tervo/LICENSING.md" \
            "share/doc/tervo/BUILD-INFO"; do
            if [[ -e "$PREFIX/$rel" ]]; then
                rm -f -- "$PREFIX/$rel"
                removed=1
            fi
        done
    fi
    # 只删除安装器自己的状态目录；pre/post 都校验路径，绝不 rm -rf 用户 prefix。
    if [[ "$STATE_DIR" == "$PREFIX/lib/tervo" && -d "$STATE_DIR" ]]; then
        rm -rf -- "$STATE_DIR"
        removed=1
    fi
    # 只清空目录，不递归删除用户可能放入的同名目录。
    for rel in \
        "share/icons/hicolor/64x64/apps" \
        "share/icons/hicolor/512x512/apps" \
        "share/icons/hicolor/64x64" \
        "share/icons/hicolor/512x512" \
        "share/icons/hicolor" \
        "share/applications" \
        "share/doc/tervo" \
        "bin"; do
        if [[ -d "$PREFIX/$rel" ]]; then
            rmdir --ignore-fail-on-non-empty -- "$PREFIX/$rel" 2>/dev/null || true
        fi
    done
    update_caches
    if [[ "$removed" -eq 1 ]]; then
        echo "已卸载 Tervo（$PREFIX）"
    else
        echo "未发现已安装的 Tervo（$PREFIX），无需卸载"
    fi
    echo "配置/Profile/主题未改动：~/.config/tervo（或 TERVO_CONFIG/TERVO_PROFILES 指定路径）"
    echo "日志未改动：默认 ~/.local/state/tervo/logs（TERVO_LOG_DIR 可覆盖）"
}

case "$MODE" in
    install) do_install ;;
    rollback) do_rollback ;;
    uninstall) do_uninstall ;;
esac
