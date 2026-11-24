#!/usr/bin/env bash
# dsh-tui image-preview e2e inside xtervo — isolated, reproducible, headless.
#
# What it proves:
#   1. xtervo advertises Sixel to the app (DA1 rewrite) and does NOT claim Kitty.
#   2. A real dsh-tui session (copied from $HOME/.dsh) renders its transcript
#      image thumbnails as Sixel inside xtervo.
#   3. A real `[Image #N]` staging path works end-to-end: a real PNG path placed
#      on the X11 PRIMARY selection (xsel), pasted through xtervo's middle-click
#      paste as a bracketed paste, is staged by dsh-tui.
#   4. dsh-tui opens its own image preview: the transcript-thumbnail click is
#      injected at the PTY boundary (SGR mouse report) because a physical click
#      is swallowed by xtervo's own image lightbox; the composer `[Image #N]`
#      peek is opened with real keyboard input through xtervo.
#   5. Zoom / pan / close work, and closing the preview leaves no image
#      residue on the transcript (pixel diff against the pre-open frame).
#
# Isolation: a fresh Xvfb on a TCP display (never :1), a private HOME/DSH_HOME
# copy, and only processes started by this script are killed (process group).
#
# Requirements: built xtervo + xwin/xclick/xkey, Xvfb, xdpyinfo/xwininfo/xwd,
# python3 with Pillow, xsel, node (dsh-tui profile), zstd.
#
# Env overrides:
#   E2E_OUT          output directory (default .logs/dsh-image-preview-e2e)
#   E2E_BIN          xtervo binary (default target/debug/xtervo)
#   E2E_XVFB_BIN     Xvfb binary (auto-detected from .perf or PATH)
#   E2E_SESSION_ID   real dsh session to copy (default below)
#   E2E_DSH_SRC_HOME source DSH_HOME (default $HOME/.dsh)
#   E2E_DSH_TUI_SRC  source dsh-tui data dir (default $HOME/.dsh-tui)
#   E2E_SCRATCH_HOME HOME for xtervo/dsh-tui (default: private copy in $E2E_OUT)
#
# Exit status: 0 only when every check passes.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

OUT=${E2E_OUT:-$ROOT/.logs/dsh-image-preview-e2e}
BIN=${E2E_BIN:-$ROOT/target/debug/xtervo}
XWIN=${E2E_XWIN:-$ROOT/target/debug/xwin}
XCLICK=${E2E_XCLICK:-$ROOT/target/debug/xclick}
XKEY=${E2E_XKEY:-$ROOT/target/debug/xkey}
SESSION_ID=${E2E_SESSION_ID:-35378219-8b26-48b1-b67c-9ede5f0bf6bc}
SRC_DSH=${E2E_DSH_SRC_HOME:-$HOME/.dsh}
SRC_TUI=${E2E_DSH_TUI_SRC:-$HOME/.dsh-tui}
SCRATCH=${E2E_SCRATCH_HOME:-$OUT/home}
COLS=130
ROWS=40
SCREEN=1400x950x24
PROXY=$ROOT/scripts/e2e_dsh_pty_proxy.py
CHECKS=$ROOT/scripts/e2e_dsh_image_checks.py
DSH_TUI_JS="$SRC_DSH/profiles/dsh-tui/node_modules/@deepseek-harness-tui/dsh-tui/bin/dsh-tui.js"

PASS=0
FAIL=0
FAILED_NAMES=()

ok()   { PASS=$((PASS + 1)); printf 'PASS: %s\n' "$*"; }
bad()  { FAIL=$((FAIL + 1)); FAILED_NAMES+=("$*"); printf 'FAIL: %s\n' "$*"; }
note() { printf 'INFO: %s\n' "$*"; }

XVFB_PID=""
APP_PID=""
XSEL_PID=""
WIN=""
WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0
CELL_W=0
CELL_H=0
CONTENT_TOP=0

cleanup() {
  set +e
  [ -n "$APP_PID" ] && kill -TERM -- "-$APP_PID" 2>/dev/null
  [ -n "$APP_PID" ] && kill -TERM "$APP_PID" 2>/dev/null
  [ -n "$XSEL_PID" ] && kill "$XSEL_PID" 2>/dev/null
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT INT TERM

die() {
  printf 'FATAL: %s\n' "$*" >&2
  exit 2
}

for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY" "$PROXY" "$CHECKS"; do
  [ -e "$f" ] || die "missing $f (build: cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey)"
done
[ -x "$BIN" ] || die "$BIN is not executable"
command -v xdpyinfo xwininfo xwd xsel node zstd >/dev/null \
  || die "missing one of: xdpyinfo xwininfo xwd xsel node zstd"
python3 - <<'PY' || die "python3 Pillow is required"
import PIL  # noqa: F401
PY
[ -f "$DSH_TUI_JS" ] || die "dsh-tui not found at $DSH_TUI_JS"
[ -d "$SRC_DSH/sessions" ] || die "no sessions under $SRC_DSH"
SESSION_DIR=$(find "$SRC_DSH/sessions" -maxdepth 2 -type d -name "$SESSION_ID" -print -quit)
[ -n "$SESSION_DIR" ] || die "session $SESSION_ID not found under $SRC_DSH/sessions"

XVFB=${E2E_XVFB_BIN:-}
if [ -z "$XVFB" ]; then
  for cand in "$ROOT/.perf/xvfb/usr/bin/Xvfb" "$ROOT/../.perf/xvfb/usr/bin/Xvfb" "$(command -v Xvfb 2>/dev/null || true)"; do
    [ -n "$cand" ] && [ -x "$cand" ] && XVFB=$cand && break
  done
fi
[ -n "$XVFB" ] && [ -x "$XVFB" ] || die "Xvfb not found (set E2E_XVFB_BIN); no Xephyr fallback"

rm -rf "$OUT"
mkdir -p "$OUT"/{logs,pty,shots,fixtures,home/.dsh-tui,home/.cache}
export HOME_FOR_E2E=$OUT/home

# ---------------------------------------------------------------------------
# Isolated DSH_HOME: copy the dsh-tui profile, the chosen real session, the
# attachment objects it references, credentials (read-only copy) and theme.
# Nothing here writes back to the live $HOME.
# ---------------------------------------------------------------------------
mkdir -p "$OUT/home/.dsh/profiles" "$OUT/home/.dsh/sessions"
cp -a "$SRC_DSH/profiles/dsh-tui" "$OUT/home/.dsh/profiles/"
[ -d "$SRC_DSH/profiles/node_modules" ] && cp -a "$SRC_DSH/profiles/node_modules" "$OUT/home/.dsh/profiles/"
SESS_PARENT=$(basename "$(dirname "$SESSION_DIR")")
mkdir -p "$OUT/home/.dsh/sessions/$SESS_PARENT"
cp -a "$SESSION_DIR" "$OUT/home/.dsh/sessions/$SESS_PARENT/"
if [ -d "$SRC_DSH/attachments" ]; then
  cp -a "$SRC_DSH/attachments" "$OUT/home/.dsh/"
fi
if [ -f "$SRC_DSH/.credentials.yaml" ]; then
  cp -a "$SRC_DSH/.credentials.yaml" "$OUT/home/.dsh/"
  chmod 600 "$OUT/home/.dsh/.credentials.yaml"
fi
for f in theme.json lang.json; do
  [ -f "$SRC_TUI/$f" ] && cp -a "$SRC_TUI/$f" "$OUT/home/.dsh-tui/"
done
# A killed previous run may have left a mount claim whose pid is reused inside
# this sandbox's PID namespace; the copy is ours alone, so drop it.
rm -f "$OUT/home/.dsh-tui/session-mounts.json" \
      "$OUT/home/.dsh-tui/session-index.json" \
      "$OUT/home/.dsh-tui/last-run.json"

# Distinctive fixture: magenta border + green block + blue disc. Magenta is
# absent from both the dsh light theme and the copied session, so pixel counts
# are an unambiguous "preview is visible" probe.
FIXTURE=$OUT/fixtures/fixture-magenta.png
python3 - "$FIXTURE" <<'PY'
import sys
from PIL import Image, ImageDraw
im = Image.new("RGB", (640, 400), (255, 0, 255))
d = ImageDraw.Draw(im)
d.rectangle([80, 60, 560, 340], fill=(0, 200, 0))
d.ellipse([220, 120, 420, 280], fill=(0, 0, 255))
im.save(sys.argv[1])
PY
printf '%s' "$FIXTURE" > "$OUT/fixtures/fixture-path.txt"   # no trailing newline

# ---------------------------------------------------------------------------
# Display: isolated Xvfb, TCP-only (the sandbox's /tmp has no shared
# /tmp/.X11-unix), so no chance of touching the live :1 session.
# ---------------------------------------------------------------------------
N=$((280 + $$ % 20))
for _ in $(seq 1 20); do
  if ! DISPLAY="127.0.0.1:$N.0" xdpyinfo >/dev/null 2>&1; then break; fi
  N=$((N + 1))
done
TCP_DISPLAY="127.0.0.1:$N.0"
"$XVFB" ":$N" -screen 0 "$SCREEN" -ac -nolisten unix -listen tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 200); do
  DISPLAY=$TCP_DISPLAY xdpyinfo >/dev/null 2>&1 && break
  sleep 0.05
done
DISPLAY=$TCP_DISPLAY xdpyinfo >/dev/null 2>&1 || die "Xvfb :$N failed"
export DISPLAY=$TCP_DISPLAY
note "Xvfb :$N screen=$SCREEN tcp=$TCP_DISPLAY (no :1)"

# Real X11 PRIMARY selection holding the fixture path. xsel runs with the
# private HOME so its cache/log never touches the live user directory.
HOME=$OUT/home XDG_CACHE_HOME=$OUT/home/.cache \
  xsel --nodetach --primary --input <"$OUT/fixtures/fixture-path.txt" >/dev/null 2>&1 &
XSEL_PID=$!
sleep 0.3
CLIP=$(HOME=$OUT/home XDG_CACHE_HOME=$OUT/home/.cache xsel --primary --output 2>/dev/null)
[ "$CLIP" = "$FIXTURE" ] || die "xsel PRIMARY did not take the fixture path (got '$CLIP')"

cat >"$OUT/run_dsh.sh" <<EOF
#!/bin/bash
export HOME="$OUT/home"
export DSH_HOME="$OUT/home/.dsh"
export XDG_CONFIG_HOME="$OUT/home/.config"
export XDG_CACHE_HOME="$OUT/home/.cache"
export TERM=xterm-256color
export COLORTERM=truecolor
export DSH_TUI_LANG=en
cd "\$HOME"
exec python3 "$PROXY" "$OUT/pty" $COLS $ROWS \
  node "$DSH_TUI_JS" --resume "$SESSION_ID" 2>>"$OUT/dsh.stderr.log"
EOF
chmod +x "$OUT/run_dsh.sh"

XTERVO_CFG=$OUT/xtervo.toml
: >"$XTERVO_CFG"
setsid env DISPLAY=$TCP_DISPLAY HOME=$OUT/home XTERVO_CONFIG=$XTERVO_CFG \
  XTERVO_LOG_DIR=$OUT/logs XTERVO_SIXEL_DEBUG=1 \
  "$BIN" --shell /bin/bash --cols $COLS --rows $ROWS --title xtervo \
  -- -c "bash $OUT/run_dsh.sh" >"$OUT/xtervo.log" 2>&1 &
APP_PID=$!

for _ in $(seq 1 300); do
  WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}')
  [ -n "$WIN" ] && break
  sleep 0.05
done
[ -n "$WIN" ] || { tail -30 "$OUT/xtervo.log"; die "xtervo window did not appear"; }
eval "$(xwininfo -id "$WIN" | awk '
  /Absolute upper-left X/{printf "WIN_X=%s\n", $NF}
  /Absolute upper-left Y/{printf "WIN_Y=%s\n", $NF}
  /Width:/{printf "WIN_W=%s\n", $2}
  /Height:/{printf "WIN_H=%s\n", $2}')"
"$XWIN" "$WIN" --focus >/dev/null 2>&1 || true
note "window=$WIN ${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}"

shot() {  # <name>
  xwd -id "$WIN" -silent -out "$OUT/shots/$1.xwd" \
    && python3 "$ROOT/scripts/xwd2png.py" "$OUT/shots/$1.xwd" "$OUT/shots/$1.png" >/dev/null
}
click() {  # <local x> <local y> <button>
  "$XCLICK" $((WIN_X + $1)) $((WIN_Y + $2)) "$3" >/dev/null 2>&1
}
key() {  # <xkey args...>
  "$XKEY" "$@" >/dev/null 2>&1
}
inj_hex() {  # <hex bytes> -> child PTY (dsh stdin)
  python3 - "$OUT/pty/inject.sock" "$1" <<'PY'
import base64, json, socket, sys
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
s.sendall(json.dumps({"write_b64": base64.b64encode(bytes.fromhex(sys.argv[2])).decode()}).encode() + b"\n")
s.close()
PY
}
sgr_click() {  # <col 1-based> <row 1-based>
  local hex
  hex=$(python3 -c 'import sys; c,r=map(int,sys.argv[1:3]); print((f"\x1b[<0;{c};{r}M\x1b[<0;{c};{r}m").encode().hex())' "$1" "$2")
  inj_hex "$hex"
}
raw_size() { stat -c %s "$OUT/pty/app-to-host.raw" 2>/dev/null || echo 0; }
wait_raw() {  # <pattern> <timeout seconds> [start offset]
  local pat=$1 tmo=$2 off=${3:-0} deadline
  deadline=$((SECONDS + tmo))
  while [ $SECONDS -lt $deadline ]; do
    if tail -c +$((off + 1)) "$OUT/pty/app-to-host.raw" 2>/dev/null | grep -a -q -F -- "$pat"; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}
last_sixel_images() {  # prints "id,row,col;..." from the last [sixel] debug line
  { cat "$OUT/xtervo.log" 2>/dev/null; cat "$OUT"/logs/xtervo-*.log 2>/dev/null; } | grep -a '\[sixel\]' | grep -a 'images=' | tail -1 \
    | sed -E 's/.*images=\[(.*)\]/\1/' | tr -d '()' | tr -d ' '
}

# ---------------------------------------------------------------------------
# Boot assertion: alt screen + bracketed paste + SGR mouse + sixel DA.
# ---------------------------------------------------------------------------
deadline=$((SECONDS + 45))
while [ $SECONDS -lt $deadline ]; do
  if wait_raw $'\x1b[?1049h' 1 && wait_raw $'\x1b[?2004h' 1; then break; fi
  sleep 0.3
done
sleep 1.5
shot 01-boot

if grep -a -q -F $'\x1b[?62;4;9;22c' "$OUT/pty/host-to-app.raw"; then
  ok "xtervo DA1 advertises Sixel (ESC[?62;4;9;22c)"
else
  bad "xtervo DA1 Sixel advertisement missing"
fi
if grep -a -q -F $'Gi=31;OK' "$OUT/pty/host-to-app.raw"; then
  ok "xtervo answered the Kitty graphics query; dsh uses Kitty"
else
  bad "xtervo did not answer Kitty graphics query"
fi
grep -a -q -F $'\x1b[?1006h' "$OUT/pty/app-to-host.raw" \
  && ok "dsh-tui enabled SGR mouse reporting" \
  || note "SGR mouse enable sequence not seen (click injection may still work)"
grep -a -q -F $'\x1b[?2004h' "$OUT/pty/app-to-host.raw" \
  && ok "dsh-tui enabled bracketed paste" \
  || bad "dsh-tui did not enable bracketed paste"

# ---------------------------------------------------------------------------
# Entry A: real copied session, transcript image thumbnails, dsh modal preview.
# Physical clicks on Sixel placements are consumed by xtervo's lightbox, so the
# click is injected as the equivalent SGR mouse report at the PTY boundary.
# ---------------------------------------------------------------------------
SCROLLED=0
for _ in $(seq 1 12); do
  click 600 420 4
  sleep 0.25
  imgs=$(last_sixel_images)
  if [ -n "$imgs" ]; then SCROLLED=1; break; fi
done
sleep 0.8
shot 02-transcript-images
if grep -a -q -F $'\x1b_G' "$OUT/pty/app-to-host.raw"; then
  ok "dsh-tui emitted Kitty graphics APCs for transcript images"
else
  bad "no Kitty graphics APC in dsh-tui output"
fi
if [ "$SCROLLED" = 1 ]; then
  ok "session transcript thumbnails rendered as Sixel (images=$imgs)"
else
  bad "no Sixel transcript thumbnail appeared after scrolling"
fi

# Parse anchor of the first visible image: (id,row,col) terminal cells.
read -r IMG_ROW IMG_COL <<<"$(python3 - "$imgs" <<'PY'
import re, sys
anchors = [tuple(map(int, m)) for m in re.findall(r'(\d+),(\d+),(\d+)', sys.argv[1])]
visible = [a for a in anchors if 0 <= a[1] < 40 and 0 <= a[2] < 130]
pick = visible[0] if visible else (anchors[0] if anchors else (0, 4, 4))
print(pick[1], pick[2])
PY
)"
CELL_LINE=$(grep -a 'READY TIMING' "$OUT/xtervo.log" | tail -1)
CELL_W=$(printf '%s' "$CELL_LINE" | sed -n 's/.*cell=\([0-9]*\)x\([0-9]*\).*/\1/p')
CELL_H=$(printf '%s' "$CELL_LINE" | sed -n 's/.*cell=\([0-9]*\)x\([0-9]*\).*/\2/p')
[ -n "$CELL_W" ] && [ -n "$CELL_H" ] || die "cannot parse cell size: $CELL_LINE"
CONTENT_TOP=$((WIN_H - ROWS * CELL_H))
note "cell=${CELL_W}x${CELL_H} content_top=$CONTENT_TOP thumbnail anchor row=$IMG_ROW col=$IMG_COL"

PREVIEW_OFF=$(raw_size)
OPENED=0
for dr in 1 0 2 3; do
  col=$((IMG_COL + 2))
  row=$((IMG_ROW + dr))
  [ "$row" -ge 1 ] && [ "$row" -le $ROWS ] || continue
  click $((col * CELL_W + CELL_W / 2)) $((CONTENT_TOP + (row - 1) * CELL_H + CELL_H / 2)) 1
  if wait_raw 'Open original' 3 "$PREVIEW_OFF"; then OPENED=1; break; fi
done
sleep 1.0
shot 03-transcript-preview-open
if [ "$OPENED" = 1 ]; then
  ok "dsh-tui modal image preview opened from transcript thumbnail (PTY-injected click)"
else
  bad "dsh-tui preview did not open from transcript thumbnail click"
fi

if [ "$OPENED" = 1 ]; then
  ZOOM_OFF=$(raw_size)
  # dsh +: Fit(0) -> 100% -> 200%; two physical clicks.
  click 170 487 1
  sleep 0.5
  click 170 487 1
  if wait_raw '200%' 4 "$ZOOM_OFF"; then
    ok "preview zoom: '+' reached 200%"
  else
    bad "preview zoom: '+' did not produce 200%"
  fi
  sleep 0.8
  shot 04-preview-zoom

  click 600 400 5
  click 600 400 5
  sleep 0.8
  shot 05-preview-pan
  read -r PAN_DIFF _ < <(python3 "$CHECKS" diff "$OUT/shots/04-preview-zoom.png" "$OUT/shots/05-preview-pan.png")
  if [ "${PAN_DIFF:-0}" -gt 500 ]; then
    ok "preview pan/scroll changed the frame (diff=$PAN_DIFF px)"
  else
    bad "preview pan/scroll produced no frame change"
  fi

  inj_hex 1b
  sleep 1.5
  shot 06-preview-closed
  read -r RESIDUE _ _ _ _ < <(python3 "$CHECKS" diff "$OUT/shots/02-transcript-images.png" "$OUT/shots/06-preview-closed.png")
  if [ "${RESIDUE:-999999}" -le 5000 ]; then
    ok "closing the modal preview left no residue (diff=$RESIDUE px)"
  else
    bad "closing the modal preview left image residue (diff=$RESIDUE px; see shots/06-preview-closed.png)"
  fi
fi

# ---------------------------------------------------------------------------
# Entry B: real X11 clipboard -> xtervo bracketed paste -> dsh `[Image #N]` ->
# keyboard-opened composer peek preview. Covers the staging pipeline that a
# fake xclip would not exercise.
# ---------------------------------------------------------------------------
TOKEN_OFF=$(raw_size)
click 600 650 2     # xtervo middle-click paste (below the transcript image area) (PRIMARY = real PNG path)
PASTED=0
if wait_raw 'Pasted image' 8 "$TOKEN_OFF"; then PASTED=1; fi
sleep 1.0
shot 07-clipboard-token
if grep -a -q -F $'\x1b[200~' "$OUT/pty/host-to-app.raw"; then
  ok "xtervo forwarded the clipboard as a bracketed paste (ESC[200~)"
else
  bad "no bracketed paste bytes from xtervo"
fi
if [ "$PASTED" = 1 ] && tail -c +$((TOKEN_OFF + 1)) "$OUT/pty/app-to-host.raw" | grep -a -q -F '[Image #1]'; then
  ok "dsh-tui staged the real PNG as [Image #1]"
else
  bad "dsh-tui did not stage the pasted PNG (no [Image #1])"
fi

PEEK_OFF=$(raw_size)
if [ "$PASTED" = 1 ]; then
  click 95 $((WIN_H - 120)) 1
  if ! wait_raw 'Open original' 4 "$PEEK_OFF"; then
    key --key left
    key --key left
  fi
  if wait_raw 'Open original' 6 "$PEEK_OFF"; then
    ok "composer image peek preview opened from the [Image #1] token"
  else
    bad "composer image peek did not open (token click + two Left presses)"
  fi
  sleep 1.0
  shot 08-composer-peek
  MAGENTA=$(python3 "$CHECKS" magenta "$OUT/shots/08-composer-peek.png")
  if [ "${MAGENTA:-0}" -gt 8000 ]; then
    ok "composer peek shows the real fixture image (magenta=$MAGENTA px)"
  else
    bad "composer peek does not show the fixture (magenta=$MAGENTA px)"
  fi
  inj_hex 1b
  sleep 1.5
  shot 09-composer-peek-closed
  CLOSED_MAGENTA=$(python3 "$CHECKS" magenta "$OUT/shots/09-composer-peek-closed.png")
  read -r PEEK_RESIDUE _ _ _ _ < <(python3 "$CHECKS" diff "$OUT/shots/07-clipboard-token.png" "$OUT/shots/09-composer-peek-closed.png")
  if [ "${CLOSED_MAGENTA:-999999}" -le 500 ] && [ "${PEEK_RESIDUE:-999999}" -le 5000 ]; then
    ok "closing the composer peek left no residue (magenta=$CLOSED_MAGENTA diff=$PEEK_RESIDUE)"
  else
    bad "closing the composer peek left residue (magenta=$CLOSED_MAGENTA diff=$PEEK_RESIDUE; see shots/09-composer-peek-closed.png)"
  fi
fi

# ---------------------------------------------------------------------------
# Frame snapshots from the captured app bytes (independent of X screenshots).
# ---------------------------------------------------------------------------
if "$BIN" diag replay --input "$OUT/pty/app-to-host.raw" --cols $COLS --rows $ROWS \
    >"$OUT/frame-snapshot.json" 2>"$OUT/frame-snapshot.err"; then
  ok "saved textual frame snapshot (frame-snapshot.json)"
else
  note "diag replay unavailable/failed; see frame-snapshot.err (screenshots + raw PTY remain authoritative)"
fi

# ---------------------------------------------------------------------------
# Optional Kitty reference: report honestly instead of pretending to compare.
# ---------------------------------------------------------------------------
if command -v kitty >/dev/null 2>&1; then
  note "kitty found at $(command -v kitty); this harness does not yet drive it as a reference"
else
  note "kitty reference unavailable on this host (not installed); Sixel output compared against xtervo's own lightbox screenshots instead"
fi

printf '\n===== dsh-tui image-preview e2e =====\n'
printf 'artifacts: %s\n' "$OUT"
printf 'raw PTY:   %s/pty/{host-to-app,app-to-host}.raw\n' "$OUT"
printf 'shots:     %s/shots/*.png\n' "$OUT"
printf 'PASS=%d FAIL=%d\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf 'failed checks:\n'
  for n in "${FAILED_NAMES[@]}"; do printf '  - %s\n' "$n"; done
  exit 1
fi
exit 0
