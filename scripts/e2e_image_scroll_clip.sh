#!/usr/bin/env bash
# Deterministic inline-image scroll-clip regression e2e.
#
# Places a 1x3-cell-tall, 6-cell-wide Kitty image at screen row 5, then scrolls
# the normal-screen buffer by 5 lines so the image top sits one row above the
# viewport (negative anchor row, partially visible). A second scroll pushes it
# fully out.
#
# Expected fixed behaviour:
#   phase 1 (partially clipped): middle band on row 0, bottom band on row 1,
#   nothing below. The old bug drew every visible slice `-top` pixels too low,
#   so the image appeared one row down and its pixels were outside the
#   placement's row span.
#   phase 2 (scrolled fully out): the renderer marks/clears every row the image
#   actually touched; no red/green/blue band pixels may remain anywhere.
#
# Isolation: own Xvfb on TCP (never :1), temp HOME under $OUT, no dsh needed.
# Env: E2E_BIN, E2E_XWIN, E2E_XCLICK, E2E_XVFB_BIN, E2E_OUT.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/.logs/image-scroll-clip-e2e}
BIN=${E2E_BIN:-$ROOT/target/debug/xtervo}
XWIN=${E2E_XWIN:-$ROOT/target/debug/xwin}
XCLICK=${E2E_XCLICK:-$ROOT/target/debug/xclick}
COLS=40
ROWS=20
SCREEN=800x640x24

PASS=0
FAIL=0
FAILED=()
ok() { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$*"; }
bad() { FAIL=$((FAIL + 1)); FAILED+=("$*"); printf 'FAIL: %s\n' "$*"; }
note() { printf 'INFO: %s\n' "$*"; }
die() { printf 'FATAL: %s\n' "$*" >&2; exit 2; }

XVFB_PID=""
APP_PID=""
WIN=""
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0
cleanup() {
  set +e
  [ -n "$APP_PID" ] && kill -TERM -- "-$APP_PID" 2>/dev/null
  [ -n "$APP_PID" ] && kill -TERM "$APP_PID" 2>/dev/null
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT INT TERM

for f in "$BIN" "$XWIN" "$XCLICK"; do [ -e "$f" ] || die "missing $f"; done
command -v xdpyinfo xwininfo xwd >/dev/null || die "missing X11 utilities"
python3 -c 'import PIL' || die "python3 Pillow required"

XVFB=${E2E_XVFB_BIN:-}
if [ -z "$XVFB" ]; then
  for cand in "$ROOT/.perf/xvfb/usr/bin/Xvfb" "$ROOT/../.perf/xvfb/usr/bin/Xvfb" "$(command -v Xvfb 2>/dev/null || true)"; do
    [ -n "$cand" ] && [ -x "$cand" ] && XVFB=$cand && break
  done
fi
[ -n "$XVFB" ] && [ -x "$XVFB" ] || die "Xvfb not found (set E2E_XVFB_BIN)"

if [ -n "${E2E_OUT:-}" ] && [ -e "$OUT" ]; then
  die "E2E_OUT=$OUT 已存在；拒绝删除调用者目录。请换一个新目录或先手动清理。"
fi
rm -rf -- "$OUT"
mkdir -p "$OUT"/{shots,home,logs}

# --- payload: Kitty transmit chunks, placement, then scroll lines -----------
python3 - "$OUT/payload-partial.bin" "$OUT/payload-away.bin" <<'PYPAYLOAD'
import base64
import sys

W, H = 54, 66  # 6 cells x 3 rows at 9x22
CELL_H = 22
rgba = bytearray()
for y in range(H):
    band = min(y // CELL_H, 2)
    color = [(255, 0, 0, 255), (0, 255, 0, 255), (0, 0, 255, 255)][band]
    rgba.extend(bytes(color) * W)
b64 = base64.b64encode(bytes(rgba)).decode()
chunks = [b64[i:i + 4096] for i in range(0, len(b64), 4096)]


def apc(control, payload=""):
    return b"\x1b_G" + control.encode() + b";" + payload.encode() + b"\x1b\\"


transmit = bytearray()
for i, chunk in enumerate(chunks):
    more = 0 if i + 1 == len(chunks) else 1
    if i == 0:
        control = f"a=t,t=d,f=32,s={W},v={H},i=7,q=2,m={more}"
    else:
        control = f"m={more},q=2"
    transmit.extend(apc(control, chunk))

partial = bytearray()
# No clear-screen here: a `2J` still in the core's image-clear tail window can
# be re-detected on the next text chunk and wipe the placement (separate bug).
partial += transmit
partial += b"\x1b[5;5H"  # cursor to screen row 5 (0-based 4), col 5
partial += apc("a=p,i=7,p=1,c=6,r=3,z=-1,C=1,q=2")
partial += b"\x1b[20;1H"  # bottom row
partial += b"\r\n" * 5
partial += b"PARTIAL-DONE"

away = bytearray()
away += b"\x1b[20;1H" + b"\r\n" * 6 + b"AWAY-DONE"

open(sys.argv[1], "wb").write(partial)
open(sys.argv[2], "wb").write(away)
print("payloads written")
PYPAYLOAD

cat > "$OUT/run_image.sh" <<'RUNSCRIPT'
#!/bin/bash
export HOME="__HOME__"
export XDG_CACHE_HOME="$HOME/.cache"
cat "__OUT__/payload-partial.bin"
sleep 3
cat "__OUT__/payload-away.bin"
sleep 300
RUNSCRIPT
sed -i "s|__HOME__|$OUT/home|g; s|__OUT__|$OUT|g" "$OUT/run_image.sh"
chmod +x "$OUT/run_image.sh"

N=$((360 + $$ % 20))
for _ in $(seq 1 20); do
  DISPLAY="127.0.0.1:$N.0" xdpyinfo >/dev/null 2>&1 || break
  N=$((N + 1))
done
TCP="127.0.0.1:$N.0"
"$XVFB" ":$N" -screen 0 "$SCREEN" -ac -nolisten unix -listen tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 200); do
  DISPLAY=$TCP xdpyinfo >/dev/null 2>&1 && break
  sleep 0.05
done
DISPLAY=$TCP xdpyinfo >/dev/null 2>&1 || die "Xvfb failed"
export DISPLAY=$TCP

setsid env DISPLAY=$TCP HOME=$OUT/home XTERVO_CONFIG=$OUT/xtervo.toml \
  XTERVO_LOG_DIR=$OUT/logs XTERVO_SIXEL_DEBUG=1 \
  "$BIN" --shell /bin/bash --cols $COLS --rows $ROWS --title xtervo \
  -- -c "bash $OUT/run_image.sh" >"$OUT/xtervo.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 300); do
  WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}')
  [ -n "$WIN" ] && break
  sleep 0.05
done
[ -n "$WIN" ] || { tail -20 "$OUT/xtervo.log"; die "xtervo window missing"; }
eval "$(xwininfo -id "$WIN" | awk '
  /Absolute upper-left X/{printf "WIN_X=%s\n", $NF}
  /Absolute upper-left Y/{printf "WIN_Y=%s\n", $NF}
  /Width:/{printf "WIN_W=%s\n", $2}
  /Height:/{printf "WIN_H=%s\n", $2}')"
"$XWIN" "$WIN" --focus >/dev/null 2>&1 || true
shot() {
  xwd -id "$WIN" -silent -out "$OUT/shots/$1.xwd" \
    && python3 "$ROOT/scripts/xwd2png.py" "$OUT/shots/$1.xwd" "$OUT/shots/$1.png" >/dev/null
}

logs() { cat "$OUT/xtervo.log" "$OUT"/logs/xtervo-*.log 2>/dev/null; }

# Wait until the placement is visible with a negative (partially clipped) row.
deadline=$((SECONDS + 15))
until logs | grep -a -E '\[sixel\].*images=\[[^]]*,[[:space:]]*-[0-9]+,' >/dev/null 2>&1; do
  [ $SECONDS -ge $deadline ] && break
  sleep 0.2
done
sleep 1.2
shot 01-partial
CELL_LINE=$(grep -a 'READY TIMING' "$OUT/xtervo.log" | tail -1)
CELL_W=$(printf '%s' "$CELL_LINE" | sed -n 's/.*cell=\([0-9]*\)x\([0-9]*\).*/\1/p')
CELL_H=$(printf '%s' "$CELL_LINE" | sed -n 's/.*cell=\([0-9]*\)x\([0-9]*\).*/\2/p')
[ -n "$CELL_W" ] && [ -n "$CELL_H" ] || die "cannot parse cell size: $CELL_LINE"
note "cell=${CELL_W}x${CELL_H} window=${WIN_W}x${WIN_H}"

CONTENT_TOP=32
IMG_X=$((4 * CELL_W + 2))
Y0=$((CONTENT_TOP + 6))
Y1=$((CONTENT_TOP + CELL_H + 6))
Y2=$((CONTENT_TOP + 2 * CELL_H + 6))

probe() {  # <png> <x> <y> -> "r g b"
  python3 - "$1" "$2" "$3" <<'PROBE'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
print(*im.getpixel((int(sys.argv[2]), int(sys.argv[3]))))
PROBE
}
read -r R0 G0 B0 < <(probe "$OUT/shots/01-partial.png" "$IMG_X" "$Y0")
read -r R1 G1 B1 < <(probe "$OUT/shots/01-partial.png" "$IMG_X" "$Y1")
read -r R2 G2 B2 < <(probe "$OUT/shots/01-partial.png" "$IMG_X" "$Y2")
note "partial pixels: row0=$R0,$G0,$B0 row1=$R1,$G1,$B1 row2=$R2,$G2,$B2"
if [ "$G0" -gt 200 ] && [ "$R0" -lt 60 ] && [ "$B0" -lt 60 ]; then
  ok "clipped image starts at the top row with its middle color band"
else
  bad "clipped image is shifted down (row0=$R0,$G0,$B0; expected green)"
fi
if [ "$B1" -gt 200 ] && [ "$R1" -lt 60 ] && [ "$G1" -lt 60 ]; then
  ok "clipped image bottom band lands on row1"
else
  bad "clipped image bottom band missing from row1 (row1=$R1,$G1,$B1)"
fi
if [ "$B2" -gt 200 ] && [ "$R2" -lt 60 ] && [ "$G2" -lt 60 ]; then
  bad "image pixels painted below the clipped placement (row2=$R2,$G2,$B2)"
else
  ok "no image pixels below the clipped placement"
fi

# Phase 2: the script scrolls the image fully out. Wait for a *new* empty
# placement line, not for any historical `images=[]` before the image appeared.
before_sixel=$(logs | grep -a -c '\[sixel\]')
deadline=$((SECONDS + 15))
while :; do
  now_sixel=$(logs | grep -a -c '\[sixel\]')
  last_sixel=$(logs | grep -a '\[sixel\]' | tail -1)
  if [ "$now_sixel" -gt "$before_sixel" ] && printf '%s' "$last_sixel" | grep -q 'images=\[\]'; then
    break
  fi
  [ $SECONDS -ge $deadline ] && break
  sleep 0.2
done
sleep 1.2
shot 02-away
leftover=$(python3 - "$OUT/shots/02-away.png" "$CONTENT_TOP" <<'LEFTOVER'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
top = int(sys.argv[2])
count = 0
for y in range(top, im.height):
    for x in range(im.width):
        r, g, b = im.getpixel((x, y))
        if (r > 200 and g < 60 and b < 60) or (g > 200 and r < 60 and b < 60) or (b > 200 and r < 60 and g < 60):
            count += 1
print(count)
LEFTOVER
)
if [ "${leftover:-999999}" -le 20 ]; then
  ok "scrolling the clipped image out clears all image pixels (leftover=$leftover)"
else
  bad "clipped image left residue after scrolling out (leftover=$leftover px; shots/02-away)"
fi

# Whole-pane repaint oracle: resize and compare. Any band color after repaint
# means the incremental clear missed rows the image had touched.
"$XWIN" "$WIN" "$WIN_X" "$WIN_Y" $((WIN_W + 18)) "$WIN_H" >/dev/null 2>&1
sleep 1.2
"$XWIN" "$WIN" "$WIN_X" "$WIN_Y" "$WIN_W" "$WIN_H" >/dev/null 2>&1
sleep 3
shot 03-after-repaint
leftover2=$(python3 - "$OUT/shots/03-after-repaint.png" "$CONTENT_TOP" <<'LEFTOVER2'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert("RGB")
top = int(sys.argv[2])
count = 0
for y in range(top, im.height):
    for x in range(im.width):
        r, g, b = im.getpixel((x, y))
        if (r > 200 and g < 60 and b < 60) or (g > 200 and r < 60 and b < 60) or (b > 200 and r < 60 and g < 60):
            count += 1
print(count)
LEFTOVER2
)
if [ "${leftover2:-999999}" -le 20 ]; then
  ok "repainted pane has no band-colored pixels (leftover=$leftover2)"
else
  bad "repainted pane still has band-colored pixels (leftover=$leftover2)"
fi

printf '\n===== inline image scroll-clip e2e =====\n'
printf 'artifacts: %s\n' "$OUT"
printf 'shots: %s/shots/*.png\n' "$OUT"
printf 'PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  for n in "${FAILED[@]}"; do printf '  - %s\n' "$n"; done
  exit 1
fi
exit 0
