#!/usr/bin/env bash
# 验证 UI-TERM-001（IME 候选框位置）——在**你的桌面会话**里跑，不要 headless。
#
# 做的事：起一个独立实例的 Tervo（Tervo 窗口会短暂抢焦点）→ 用 XTEST 注入 "nihao"+空格
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
BIN=${BIN:-target/release/tervo}
[ -x "$BIN" ] || { echo "缺少 $BIN，先 cargo build --release -p tervo-app" >&2; exit 2; }

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
# 让 Tervo 走 fcitx
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

say "启动独立实例 Tervo（会在你桌面上新开一个窗口）"
BEFORE=$(xwininfo -root -tree 2>/dev/null | awk '/"Tervo"/{print $1}' | tr '\n' ' ')
TMP_INST=/tmp/tervo-ime-verify
rm -rf "$TMP_INST"; mkdir -p "$TMP_INST"
TERVO_KEYLOG=1 TERVO_INSTANCE_DIR="$TMP_INST" \
    "$BIN" --cols 100 --rows 30 >"$OUT/keylog.log" 2>&1 &
APP_PID=$!
cleanup() { kill "$APP_PID" 2>/dev/null || true; }
trap 'cleanup' EXIT

# 只认“启动之前不存在”的窗口，避免把输入打进你正在用的 Tervo
WID=""
for _ in $(seq 1 40); do
    for id in $(xwininfo -root -tree 2>/dev/null | awk '/"Tervo"/{print $1}'); do
        case " $BEFORE " in *" $id "*) continue ;; esac
        WID=$id; break
    done
    [ -n "$WID" ] && break
    sleep 0.25
done
[ -n "$WID" ] || { echo "Tervo 窗口没出现；日志：$OUT/keylog.log" >&2; exit 3; }
GEO=$(xwininfo -id "$WID" | awk '/Absolute upper-left X/{x=$4} /Absolute upper-left Y/{y=$4} /Width:/{w=$2} /Height:/{h=$2} END{printf "%d %d %d %d", x,y,w,h}')
read -r WX WY WW WH <<<"$GEO"
note "window=$WID geometry=${WX},${WY} ${WW}x${WH}"
for _ in $(seq 1 40); do
    xwininfo -id "$WID" 2>/dev/null | grep -q "Map State: IsViewable" && break
    sleep 0.25
done
note "map state: $(xwininfo -id "$WID" 2>/dev/null | grep 'Map State' | xargs || echo unknown)"

say "点击终端中部并输入 nihao + 空格（XTEST）"
python3 - "$WID" "$WX" "$WY" "$WW" "$WH" <<'PY'
import ctypes, ctypes.util, sys, time
wid, x, y, w, h = sys.argv[1], *map(int, sys.argv[2:6])
x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XStringToKeysym.restype = ctypes.c_ulong
x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
x11.XKeysymToKeycode.restype = ctypes.c_ubyte
x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XFlush.argtypes = [ctypes.c_void_p]
x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
x11.XRaiseWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
# 窗口可能刚 map / 被 WM reparent：忽略 X 错误，别让 BadMatch 直接退出
ERRHANDLER = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)
def _noop_err(dpy, event):
    return 0
_handler = ERRHANDLER(_noop_err)
x11.XSetErrorHandler.restype = ctypes.c_void_p
x11.XSetErrorHandler.argtypes = [ERRHANDLER]
x11.XSetErrorHandler(_handler)
xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]

d = x11.XOpenDisplay(None)
win = int(wid, 16)
x11.XRaiseWindow(d, win)
x11.XSetInputFocus(d, win, 1, 0)   # 失败也继续：点击同样会把焦点给窗口
x11.XSync(d, 0); time.sleep(0.3)
cx, cy = x + w // 2, y + h // 2
xtst.XTestFakeMotionEvent(d, -1, cx, cy, 0)
xtst.XTestFakeButtonEvent(d, 1, 1, 0); xtst.XTestFakeButtonEvent(d, 1, 0, 0)
x11.XSync(d, 0); time.sleep(0.4)

kc = lambda name: x11.XKeysymToKeycode(d, x11.XStringToKeysym(name.encode()))
def tap(name, delay=0.06):
    k = kc(name)
    xtst.XTestFakeKeyEvent(d, k, 1, 0); x11.XFlush(d); time.sleep(delay)
    xtst.XTestFakeKeyEvent(d, k, 0, 0); x11.XFlush(d); time.sleep(delay)

for ch in "nihao":
    tap(ch)
time.sleep(0.4)   # 等候选窗出现
x11.XSync(d, 0)
PY

# 候选窗还开着时先截图
sleep 0.3
note "截图是整屏（含其他窗口）；只看候选窗位置，看完可删 .logs/ime-verify"
xwd -root -silent -out "$OUT/root.xwd" 2>/dev/null || true
python3 scripts/xwd2png.py "$OUT/root.xwd" "$OUT/root.png" >/dev/null 2>&1 || true
xwininfo -root -tree >"$OUT/windows.txt" 2>/dev/null || true

# 选词（空格），让 IME 提交
python3 - <<'PY'
import ctypes, ctypes.util, time
x11 = ctypes.CDLL(ctypes.util.find_library("X11")); xtst = ctypes.CDLL(ctypes.util.find_library("Xtst"))
x11.XOpenDisplay.restype = ctypes.c_void_p; x11.XOpenDisplay.argtypes=[ctypes.c_char_p]
x11.XStringToKeysym.restype = ctypes.c_ulong; x11.XStringToKeysym.argtypes=[ctypes.c_char_p]
x11.XKeysymToKeycode.restype = ctypes.c_ubyte; x11.XKeysymToKeycode.argtypes=[ctypes.c_void_p, ctypes.c_ulong]
x11.XFlush.argtypes=[ctypes.c_void_p]; x11.XSync.argtypes=[ctypes.c_void_p, ctypes.c_int]
xtst.XTestFakeKeyEvent.argtypes=[ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
d = x11.XOpenDisplay(None)
k = x11.XKeysymToKeycode(d, x11.XStringToKeysym(b"space"))
xtst.XTestFakeKeyEvent(d, k, 1, 0); x11.XFlush(d); time.sleep(0.08)
xtst.XTestFakeKeyEvent(d, k, 0, 0); x11.XFlush(d); x11.XSync(d, 0)
time.sleep(0.6)
PY

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
echo "截图：$OUT/root.png  日志：$OUT/keylog.log  窗口树：$OUT/windows.txt"

if [ "$KEEP" = 0 ]; then cleanup; trap - EXIT; else echo "（--keep：Tervo 保持打开，pid=$APP_PID）"; trap - EXIT; fi

if [ "$fail" -eq 0 ]; then
    echo "RESULT: PASS（UI-TERM-001 可转 implemented）"
    exit 0
else
    echo "RESULT: FAIL（$fail 项未通过；先看 $OUT/keylog.log 里有没有 [ime] 行）"
    exit 1
fi
