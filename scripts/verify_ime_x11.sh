#!/usr/bin/env bash
# 验证 UI-TERM-001（IME 候选框位置）——在**你的桌面会话**里跑，不要 headless。
#
# 做的事：起一个独立实例的 xtervo（xtervo 窗口会短暂抢焦点）→ 用 XTEST 注入 "nihao"+空格
#         → 抓 [ime] 日志、截图、找 fcitx5 候选窗几何 → 对比候选窗与 ime spot。
#
# 用法：
#   ./scripts/verify_ime_x11.sh            # 跑测试并给 PASS/FAIL
#   ./scripts/verify_ime_x11.sh --keep     # 结束后不关窗口（方便你自己看）
#
# 产物：.logs/ime-verify/{keylog.log,root.png,windows.txt,summary.txt}
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

OUT=".logs/ime-verify"
rm -rf "$OUT"; mkdir -p "$OUT"
BIN=${BIN:-target/release/xtervo}
[ -x "$BIN" ] || { echo "缺少 $BIN，先 cargo build --release -p xtervo-app" >&2; exit 2; }

say() { printf '\n== %s ==\n' "$*"; }
note() { printf '   %s\n' "$*"; }

say "环境"
: "${DISPLAY:?需要 DISPLAY（在你的桌面终端里运行）}"
note "DISPLAY=$DISPLAY  XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-?}"
HAS_NATIVE=0; HAS_FLATPAK=0
command -v fcitx5 >/dev/null && HAS_NATIVE=1
flatpak list 2>/dev/null | grep -q "org.fcitx.Fcitx5" && HAS_FLATPAK=1
note "fcitx5: native=$HAS_NATIVE flatpak=$HAS_FLATPAK"
if [ "$HAS_NATIVE" = 0 ] && [ "$HAS_FLATPAK" = 0 ]; then
    echo "没找到 fcitx5（native 或 flatpak），先安装：sudo apt install fcitx5 fcitx5-chinese-addons" >&2
    exit 2
fi
# 让 xtervo 走 fcitx
export GTK_IM_MODULE=fcitx QT_IM_MODULE=fcitx XMODIFIERS=@im=fcitx

fcitx_remote() {
    if [ "$HAS_NATIVE" = 1 ] && command -v fcitx5-remote >/dev/null; then
        fcitx5-remote "$@" 2>/dev/null && return 0
    fi
    if [ "$HAS_FLATPAK" = 1 ]; then
        flatpak run --command=fcitx5-remote org.fcitx.Fcitx5 "$@" 2>/dev/null && return 0
    fi
    return 1
}

say "启动/检查 fcitx5"
if fcitx_remote -r >/dev/null 2>&1; then
    note "fcitx5 已在运行"
else
    note "fcitx5 未运行，尝试启动…"
    if [ "$HAS_NATIVE" = 1 ]; then
        (fcitx5 -d >/dev/null 2>&1 &) || true
    else
        (flatpak run org.fcitx.Fcitx5 -d >/dev/null 2>&1 &) || true
    fi
    sleep 2
    fcitx_remote -r >/dev/null 2>&1 && note "已启动" || note "启动失败（继续尝试输入）"
fi
# 切到中文输入法（Rime/拼音）。失败不致命，后面按 keylog 判定。
fcitx_remote -o >/dev/null 2>&1 || true

say "启动独立实例 xtervo（会在你桌面上新开一个窗口）"
BEFORE=$(xwininfo -root -tree 2>/dev/null | awk '/"xtervo"/{print $1}' | tr '\n' ' ')
TMP_INST=/tmp/xtervo-ime-verify
rm -rf "$TMP_INST"; mkdir -p "$TMP_INST"
XTERVO_KEYLOG=1 XTERVO_IME_LOG=1 XTERVO_INSTANCE_DIR="$TMP_INST" \
    "$BIN" --cols 100 --rows 30 >"$OUT/keylog.log" 2>&1 &
APP_PID=$!
cleanup() { kill "$APP_PID" 2>/dev/null || true; }
trap 'cleanup' EXIT

# 只认“启动之前不存在”的窗口，避免把输入打进你正在用的 xtervo
WID=""
for _ in $(seq 1 40); do
    for id in $(xwininfo -root -tree 2>/dev/null | awk '/"xtervo"/{print $1}'); do
        case " $BEFORE " in *" $id "*) continue ;; esac
        WID=$id; break
    done
    [ -n "$WID" ] && break
    sleep 0.25
done
[ -n "$WID" ] || { echo "xtervo 窗口没出现；日志：$OUT/keylog.log" >&2; exit 3; }
GEO=$(xwininfo -id "$WID" | awk '/Absolute upper-left X/{x=$4} /Absolute upper-left Y/{y=$4} /Width:/{w=$2} /Height:/{h=$2} END{printf "%d %d %d %d", x,y,w,h}')
read -r WX WY WW WH <<<"$GEO"
note "window=$WID geometry=${WX},${WY} ${WW}x${WH}"
for _ in $(seq 1 40); do
    xwininfo -id "$WID" 2>/dev/null | grep -q "Map State: IsViewable" && break
    sleep 0.25
done
note "map state: $(xwininfo -id "$WID" 2>/dev/null | grep 'Map State' | xargs || echo unknown)"

say "把窗口挪到空闲区域并激活，然后输入 nihao"
HELPER="$PWD/scripts/_xt_helper.py"
python3 "$HELPER" --display "$DISPLAY" "$WID" "move:1700,120" "activate" || true
NEWGEO=$(xwininfo -id "$WID" | awk '/Absolute upper-left X/{x=$4} /Absolute upper-left Y/{y=$4} /Width:/{w=$2} /Height:/{h=$2} END{printf "%d %d %d %d", x,y,w,h}')
read -r WX WY WW WH <<<"$NEWGEO"
note "移动后 geometry=${WX},${WY} ${WW}x${WH}"
cx=$(( WX + WW / 2 )); cy=$(( WY + WH / 2 ))
python3 "$HELPER" --display "$DISPLAY" "$WID" "click:$cx,$cy" "type:nihao" "sleep:0.6" || true

# 候选窗还开着时截图
xwd -root -silent -out "$OUT/root.xwd" 2>/dev/null || true
python3 scripts/xwd2png.py "$OUT/root.xwd" "$OUT/root.png" >/dev/null 2>&1 || true
xwininfo -root -tree >"$OUT/windows.txt" 2>/dev/null || true

say "尝试三种上屏方式（空格 / 数字 1 / 点第一个候选）"
try_commit() {
    local before
    before=$(grep -acE '\[ime\] "' "$OUT/keylog.log" 2>/dev/null || true)
    python3 "$HELPER" --display "$DISPLAY" "$WID" "$@" >/dev/null 2>&1 || true
    sleep 0.6
    local after
    after=$(grep -acE '\[ime\] "' "$OUT/keylog.log" 2>/dev/null || true)
    [ "${after:-0}" -gt "${before:-0}" ]
}
found=0
if try_commit "key:space"; then found=1; note "空格上屏成功"; fi
if [ "$found" = 0 ] && try_commit "type:nihao" "sleep:0.4" "key:1"; then found=1; note "数字 1 上屏成功"; fi
if [ "$found" = 0 ]; then
    # 点候选窗第一个候选项（几何在下面解析，这里用当前窗口树里最小的 fcitx 窗口）
    CANDID=$(awk -F'"' '/[Ff]citx/{print $1}' "$OUT/windows.txt" | awk '{print $1}' | head -1)
    if [ -n "$CANDID" ]; then
        CINFO=$(xwininfo -id "$CANDID" 2>/dev/null || true)
        CXS=$(echo "$CINFO" | awk '/Absolute upper-left X/{print $4}')
        CYS=$(echo "$CINFO" | awk '/Absolute upper-left Y/{print $4}')
        CHH=$(echo "$CINFO" | awk '/Height:/{print $2}')
        if [ -n "$CXS" ] && [ -n "$CYS" ]; then
            if try_commit "type:nihao" "sleep:0.4" "click:$((CXS + 40)),$((CYS + ${CHH:-80} * 3 / 5))"; then
                found=1; note "点击候选上屏成功"
            fi
        fi
    fi
fi

say "解析日志与窗口"
CJK=$(grep -aE '\[ime\] "' "$OUT/keylog.log" | grep -aP '[^\x00-\x7F]' | tail -1 || true)
SPOT=$(grep -a "\[ime\] spot" "$OUT/keylog.log" | tail -1 || true)
echo "$SPOT" | sed 's/^/   spot: /' >>"$OUT/summary.txt"

# 找 fcitx 候选窗（几何最小、可见）
CAND=""
while read -r id; do
    [ -z "$id" ] && continue
    info=$(xwininfo -id "$id" 2>/dev/null) || continue
    cw=$(echo "$info" | awk '/Width:/{print $2}')
    chh=$(echo "$info" | awk '/Height:/{print $2}')
    [ -z "$cw" ] && continue
    [ "$cw" -lt 20 ] && continue
    CAND="$id $cw $chh"
done < <(awk -F'"' '/[Ff]citx/{print $1}' "$OUT/windows.txt" | awk '{print $1}' | head -5)

say "判定"
fail=0
ok()  { printf 'PASS  %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fail=$((fail+1)); }

if [ -n "$CJK" ]; then ok "IME 提交中文：$(printf '%s' "$CJK" | tail -c 60)"; else bad "keylog 没有中文提交（输入法未生效）"; fi
if [ -n "$SPOT" ]; then ok "有 ime spot 日志：$(printf '%s' "$SPOT" | sed -E 's/^\[ime\] spot //' | cut -c1-60)"; else bad "没有 [ime] spot 日志"; fi

if [ -n "$CAND" ]; then
    set -- $CAND
    cand_id=$1; cand_w=$2; cand_h=$3
    cinfo=$(xwininfo -id "$cand_id")
    cxs=$(echo "$cinfo" | awk '/Absolute upper-left X/{print $4}')
    cys=$(echo "$cinfo" | awk '/Absolute upper-left Y/{print $4}')
    sx=$(printf '%s' "$SPOT" | sed -nE 's/.*win=([-0-9.]+),([-0-9.]+).*/\1/p')
    sy=$(printf '%s' "$SPOT" | sed -nE 's/.*win=([-0-9.]+),([-0-9.]+).*/\2/p')
    printf 'candidate=%s at %s,%s %sx%s | spot=%s,%s\n' "$cand_id" "$cxs" "$cys" "$cand_w" "$cand_h" "$sx" "$sy" >>"$OUT/summary.txt"
    if [ -n "$sx" ] && awk -v cx="$cxs" -v cy="$cys" -v sx="$sx" -v sy="$sy" 'BEGIN{
        dx=cx-sx; dy=cy-sy; ax=(dx<0?-dx:dx); ay=(dy<0?-dy:dy);
        exit !(ax<=400 && ay<=400)}'; then
        ok "候选窗在光标附近（candidate ${cxs},${cys} vs spot ${sx},${sy}）"
    else
        bad "候选窗离光标过远（candidate ${cxs},${cys} vs spot ${sx},${sy}）"
    fi
else
    bad "没找到 fcitx 候选窗（输入法可能没弹窗，或候选窗在截图之后才出现）"
fi

echo
cat "$OUT/summary.txt" 2>/dev/null || true
if [ "$fail" -gt 0 ]; then
    echo "--- 诊断 ---"
    echo "XMODIFIERS=${XMODIFIERS:-<unset>} GTK_IM_MODULE=${GTK_IM_MODULE:-<unset>}"
    (flatpak ps 2>/dev/null | grep -i fcitx) || echo "flatpak: 没有 fcitx 进程"
    pgrep -a fcitx5 2>/dev/null || echo "native: 没有 fcitx5 进程"
    awk -F'"' '/[Ff]citx/{print "  window: "$2" id="$1}' "$OUT/windows.txt" 2>/dev/null | head -5
    echo "--- keylog 里所有 [ime] 行 ---"
    grep -a "\[ime\]" "$OUT/keylog.log" | head -5 || echo "（没有）"
fi
echo "截图：$OUT/root.png  日志：$OUT/keylog.log  窗口树：$OUT/windows.txt"

if [ "$KEEP" = 0 ]; then cleanup; trap - EXIT; else echo "（--keep：xtervo 保持打开，pid=$APP_PID）"; trap - EXIT; fi

if [ "$fail" -eq 0 ]; then
    echo "RESULT: PASS（UI-TERM-001 可转 implemented）"
    exit 0
else
    echo "RESULT: FAIL（$fail 项未通过；先看 $OUT/keylog.log 里有没有 [ime] 行）"
    exit 1
fi
