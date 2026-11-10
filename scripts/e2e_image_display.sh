#!/usr/bin/env bash
# 图片显示 GUI e2e：Sixel 可见性、黑底 cell 图层、滚动部分越界、灯箱首次点击/Fit/100%。
# 只使用隔离 Xvfb；只 kill 本脚本启动的 Xvfb/Tervo；不碰 :1。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/image-display-e2e}
DISPLAY_NUM=${E2E_DISPLAY:-:$((240 + $$ % 10))}
SCREEN=${E2E_SCREEN:-1280x900x24}
BIN=${E2E_BIN:-$ROOT/target/debug/tervo}
XVFB=${E2E_XVFB_BIN:-$ROOT/.perf/xvfb/usr/bin/Xvfb}
XWIN=${E2E_XWIN:-$ROOT/target/debug/xwin}
XCLICK=${E2E_XCLICK:-$ROOT/target/debug/xclick}
XKEY=${E2E_XKEY:-$ROOT/target/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
  [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done
[ -x "$XVFB" ] || { echo "缺少 Xvfb: $XVFB" >&2; exit 2; }
command -v xdpyinfo xwininfo xwd >/dev/null || { echo "缺少 xdpyinfo/xwininfo/xwd" >&2; exit 2; }

rm -rf "$OUT"; mkdir -p "$OUT/home" "$OUT/logs"
FIXTURE="$OUT/image.six"
python3 - "$FIXTURE" <<'PYGEN'
import sys
p=sys.argv[1]
W,H=240,140
out=[b'\x1bPq"1;1;%d;%d#0;2;100;0;0#1;2;0;0;100'%(W,H)]
for _y in range(0,H,20):
    out.append(b'#0!'+str(W).encode()+b'~$-')
    out.append(b'#1!'+str(W).encode()+b'~$-')
out.append(b'\x1b\\')
open(p,'wb').write(b''.join(out))
PYGEN
cat > "$OUT/show.sh" <<'SHOW'
printf '\033[2J\033[1;1H'
cat 'FIXTURE_PLACEHOLDER'
printf '\r\nIMAGE-MARKER\r\n'
sleep 30
SHOW
sed -i "s#FIXTURE_PLACEHOLDER#$FIXTURE#" "$OUT/show.sh"
cat > "$OUT/blackbg.sh" <<'BLACK'
printf '\033[2J\033[1;1H\033[40m                    \033[1;1H'
cat 'FIXTURE_PLACEHOLDER'
printf '\033[0m\r\nBLACKBG-MARKER\r\n'
sleep 30
BLACK
sed -i "s#FIXTURE_PLACEHOLDER#$FIXTURE#" "$OUT/blackbg.sh"
cat > "$OUT/scroll.sh" <<'SCROLL'
printf '\033[2J\033[1;1H'
cat 'FIXTURE_PLACEHOLDER'
printf '\r\n'
seq 1 5
printf 'SCROLL-MARKER\r\n'
sleep 30
SCROLL
sed -i "s#FIXTURE_PLACEHOLDER#$FIXTURE#" "$OUT/scroll.sh"

XVFB_PID=""; APP_PID=""; WIN=""
cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT
# 沙箱里的 /tmp 是每次命令新建的 tmpfs，没有 root 所有的 /tmp/.X11-unix；
# 用 TCP-only Xvfb + 127.0.0.1 DISPLAY，避免依赖 Unix socket 目录。
TCP_DISPLAY="127.0.0.1:${DISPLAY_NUM#:}.0"
if DISPLAY=$TCP_DISPLAY xdpyinfo >/dev/null 2>&1; then
  echo "FAIL: display $TCP_DISPLAY 已被占用" >&2; exit 1
fi
"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten unix -listen tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do DISPLAY=$TCP_DISPLAY xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
export DISPLAY=$TCP_DISPLAY

start_app() { # <script>
  local script=$1
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  sleep 0.25
  ( HOME="$OUT/home" TERVO_CONFIG="$OUT/config.toml" TERVO_LOG_DIR="$OUT/logs" \
    TERVO_SIXEL_DEBUG=1 "$BIN" --shell /bin/bash --cols 100 --rows 28 --title Tervo \
    -- -c "bash '$script'" >"$OUT/app.log" 2>&1 & echo $! >"$OUT/app.pid" )
  APP_PID=$(cat "$OUT/app.pid")
  WIN=""
  for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"Tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
  done
  [ -n "$WIN" ] || { echo "FAIL: Tervo 窗口未出现（display=$DISPLAY）"; tail -20 "$OUT/app.log"; exit 1; }
  sleep 0.8
  ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
  ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
  W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
  H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
  echo "window=$WIN ${W}x${H} at $ABS_X,$ABS_Y"
}

shot() { xwd -id "$WIN" -silent -out "$OUT/$1.xwd"; python3 scripts/xwd2png.py "$OUT/$1.xwd" "$OUT/$1.png" >/dev/null; }
type_cmd() { "$XKEY" "$1" >/dev/null 2>&1; "$XKEY" --key enter >/dev/null 2>&1; }
click() { "$XCLICK" "$1" "$2" "${3:-1}" >/dev/null 2>&1; sleep 0.25; }
count_colors() {
python3 - "$1" <<'PYCOUNT'
from PIL import Image
import sys
im=Image.open(sys.argv[1]).convert('RGB')
r=b=0
for rr,gg,bb in im.getdata():
    if rr>200 and gg<70 and bb<70: r+=1
    if bb>200 and rr<70 and gg<70: b+=1
print(r,b)
PYCOUNT
}
image_center() { # screenshot -> absolute root x/y of red/blue bbox
python3 - "$1" "$ABS_X" "$ABS_Y" <<'PYCENTER'
from PIL import Image
import sys
im=Image.open(sys.argv[1]).convert('RGB')
xs=[]; ys=[]
for y in range(im.height):
    for x in range(im.width):
        r,g,b=im.getpixel((x,y))
        if (r>200 and g<70 and b<70) or (b>200 and r<70 and g<70):
            xs.append(x); ys.append(y)
if not xs:
    raise SystemExit(2)
print(int((min(xs)+max(xs))/2)+int(sys.argv[2]), int((min(ys)+max(ys))/2)+int(sys.argv[3]))
PYCENTER
}

assert_img() {
  read -r R B < <(count_colors "$1")
  if [ "$R" -lt "$2" ] || [ "$B" -lt "$3" ]; then
    echo "FAIL: $4 red=$R blue=$B (need >=$2/$3)"; exit 1
  fi
  echo "PASS: $4 red=$R blue=$B"
}

# 1) 正常显示
start_app "$OUT/show.sh"
shot 01-image
assert_img "$OUT/01-image.png" 8000 8000 "Sixel visible"

# 2) 黑色 backing cell 不能盖住图片
start_app "$OUT/blackbg.sh"
shot 02-blackbg
assert_img "$OUT/02-blackbg.png" 8000 8000 "image above black cell background"

# 3) 灯箱第一次点击：Fit 显示原图并放大
start_app "$OUT/show.sh"
shot 03-image-before-click
read -r IX IY < <(image_center "$OUT/03-image-before-click.png")
click "$IX" "$IY"
sleep 0.8
shot 03-lightbox-fit
assert_img "$OUT/03-lightbox-fit.png" 15000 15000 "lightbox Fit first click"
grep -q '\[image\] preview open id=' "$OUT/logs/"tervo-*.log || { echo "FAIL: 没有 image preview 日志"; exit 1; }
FIT_RED=$(count_colors "$OUT/03-lightbox-fit.png" | awk '{print $1}')

# 4) 100% 应缩小到原图
click $((ABS_X+370)) $((ABS_Y+H-34))
sleep 0.6
shot 04-lightbox-100
NATIVE_RED=$(count_colors "$OUT/04-lightbox-100.png" | awk '{print $1}')
if [ "$NATIVE_RED" -ge "$FIT_RED" ]; then
  echo "FAIL: 100% 没有缩小 fit_red=$FIT_RED native_red=$NATIVE_RED"; exit 1
fi
echo "PASS: 100% smaller than Fit ($NATIVE_RED < $FIT_RED)"

# 5) 图片部分越界 + 首次点击
start_app "$OUT/scroll.sh"
click 500 400 4; click 500 400 4; click 500 400 4
sleep 0.6
shot 05-scroll-partial
assert_img "$OUT/05-scroll-partial.png" 800 800 "partial image still visible after scroll"
read -r IX IY < <(image_center "$OUT/05-scroll-partial.png")
click "$IX" "$IY"
sleep 0.8
shot 06-lightbox-partial
assert_img "$OUT/06-lightbox-partial.png" 15000 15000 "lightbox partial first click"
grep -q '\[image\] preview open id=' "$OUT/logs/"tervo-*.log || { echo "FAIL: partial 点击没有 source"; exit 1; }

echo "PASS: image display e2e; shots=$OUT"
