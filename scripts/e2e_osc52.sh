#!/usr/bin/env bash
# OSC 52 + X11 PRIMARY 的 headless GUI 验收。
#
# 覆盖：
#   1. OSC52 set c/p → 系统剪贴板 / PRIMARY；query 回包到 PTY；clear 清空；
#   2. copy-on-select 写 PRIMARY，中键优先 PRIMARY、清空后回退普通剪贴板；
#   3. osc52 = "deny" 时两个方向都拒绝。
#
# 安全/隔离：只使用 headless Xvfb（默认 :113），只 kill 本脚本启动的
# Xvfb/app/xsel 进程；不使用 Xephyr、不碰 :1、不 pkill。
#
# 前置：
#   CARGO_HOME=... CARGO_TARGET_DIR=$PWD/target \
#     cargo build -p tervo-app --bin tervo --bin xclick
# 依赖：xsel、xwininfo、Xvfb（或 E2E_XVFB_BIN 指向 .perf/xvfb/usr/bin/Xvfb）。
#
# 用法：
#   E2E_XVFB_BIN=/path/to/Xvfb ./scripts/e2e_osc52.sh
#   E2E_DISPLAY=:113 E2E_OUT=.logs/osc52 ./scripts/e2e_osc52.sh

set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

E2E_DISPLAY=${E2E_DISPLAY:-:113}
E2E_SCREEN=${E2E_SCREEN:-1280x800x24}
E2E_OUT=${E2E_OUT:-$ROOT/.logs/osc52-gui}
E2E_BIN=${E2E_BIN:-$ROOT/target/debug/tervo}
XCLICK=${XCLICK:-$ROOT/target/debug/xclick}
XVFB=${E2E_XVFB_BIN:-Xvfb}

if command -v "$XVFB" >/dev/null 2>&1; then
  XVFB=$(command -v "$XVFB")
elif [ -x "$ROOT/.perf/xvfb/usr/bin/Xvfb" ]; then
  XVFB="$ROOT/.perf/xvfb/usr/bin/Xvfb"
fi
for tool in xsel xwininfo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "缺少依赖：$tool" >&2; exit 2; }
done
[ -x "$E2E_BIN" ] || { echo "找不到 $E2E_BIN（先 cargo build --bin tervo）" >&2; exit 2; }
[ -x "$XCLICK" ] || { echo "找不到 $XCLICK（先 cargo build --bin xclick）" >&2; exit 2; }
[ -x "$XVFB" ] || { echo "找不到 Xvfb：$XVFB（用 E2E_XVFB_BIN 指定）" >&2; exit 2; }

# 只清本脚本自己的三个输出目录，不对整个 E2E_OUT 做递归删除。
rm -rf "$E2E_OUT/osc52-ops" "$E2E_OUT/primary" "$E2E_OUT/deny"
mkdir -p "$E2E_OUT"
XVFB_PID=""
APP_PID=""
FAIL=0

cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

if DISPLAY=$E2E_DISPLAY xdpyinfo >/dev/null 2>&1; then
  echo "FAIL: display $E2E_DISPLAY 已被占用（不抢占别人的 display）" >&2
  exit 1
fi
"$XVFB" "$E2E_DISPLAY" -screen 0 "$E2E_SCREEN" -ac -nolisten tcp \
  >"$E2E_OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do DISPLAY=$E2E_DISPLAY xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
DISPLAY=$E2E_DISPLAY xdpyinfo >/dev/null 2>&1 || { echo "FAIL: Xvfb $E2E_DISPLAY 未启动"; exit 1; }
export DISPLAY=$E2E_DISPLAY

say() { echo "== $*"; }
check() { # <desc> <cmd...>
  local desc=$1; shift
  if "$@"; then echo "PASS: $desc"; else echo "FAIL: $desc"; FAIL=1; fi
}
log_has() { grep -q -- "$1" "$2"; }
wait_until() { # <秒> <cmd...>
  local secs=$1; shift
  local end=$((SECONDS + secs))
  while [ $SECONDS -lt $end ]; do "$@" >/dev/null 2>&1 && return 0; sleep 0.1; done
  return 1
}
wait_file_tail() { # <文件> <起始字节(1-based)> <期望全文>
  local file=$1 off=$2 want=$3 end=$((SECONDS + 5))
  while [ $SECONDS -lt $end ]; do
    [ "$(tail -c +"$off" "$file" 2>/dev/null)" = "$want" ] && return 0
    sleep 0.1
  done
  return 1
}
start_app() { # <dir> <osc52> <pane.sh> [额外 env...]
  local dir=$1 mode=$2 script=$3; shift 3
  cat > "$dir/cfg.toml" <<EOF
version = 1
osc52 = "$mode"
EOF
  chmod +x "$script"
  env "$@" DISPLAY=$E2E_DISPLAY XMODIFIERS=@im=none TERVO_LOG_DIR="$dir" \
    TERVO_CONFIG="$dir/cfg.toml" SLINT_BACKEND=winit-software \
    "$E2E_BIN" --shell /bin/sh -- -c "$script $dir" >"$dir/parent.log" 2>&1 &
  APP_PID=$!
  for _ in $(seq 1 100); do
    DISPLAY=$E2E_DISPLAY xwininfo -root -tree 2>/dev/null | grep -qi '"Tervo"' && return 0
    sleep 0.1
  done
  return 1
}
stop_app() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
  wait "$APP_PID" 2>/dev/null
  APP_PID=""
}
app_log() { ls -t "$1"/tervo-*.log 2>/dev/null | head -1; }
window_geom() { # -> "x y w h"
  local id
  id=$(DISPLAY=$E2E_DISPLAY xwininfo -root -tree 2>/dev/null | grep -i '"Tervo"' | awk 'NR==1{print $1}')
  [ -z "$id" ] && return 1
  DISPLAY=$E2E_DISPLAY xwininfo -id "$id" 2>/dev/null | awk '
    /Absolute upper-left X/{x=$NF}
    /Absolute upper-left Y/{y=$NF}
    /Width:/{w=$2}
    /Height:/{h=$2}
    END{if (w) print x, y, w, h}'
}

# ---------------------------------------------------------------- 1) OSC 52 ops
say "phase 1: OSC52 set/query/clear"
D1=$E2E_OUT/osc52-ops
mkdir -p "$D1"
cat > "$D1/pane.sh" <<'EOS'
stty raw -echo
printf '\033]52;c;aGVsbG8tb3NjNTI=\007'   # hello-osc52
printf '\033]52;p;cHJpbWFyeS1vc2M1Mg==\007' # primary-osc52
sleep 1
printf '\033]52;c;?\007'
timeout --foreground 3 cat > "$1/query.bin"  # 前台：同进程组，raw PTY 才读得到
printf '\033]52;c;\007'                     # clear clipboard
sleep 30
EOS
start_app "$D1" allow "$D1/pane.sh" || { echo "FAIL: app 未启动"; exit 1; }
sleep 3
CLIP=$(timeout 2 xsel -b -o 2>/dev/null)
PRIM=$(timeout 2 xsel -p -o 2>/dev/null)
check "OSC52 c 写系统剪贴板" [ "$CLIP" = "hello-osc52" ]
check "OSC52 p 写 PRIMARY" [ "$PRIM" = "primary-osc52" ]
check "query 回包含原文本" wait_until 6 grep -q "aGVsbG8tb3NjNTI=" "$D1/query.bin"
check "clear 清空剪贴板" wait_until 6 bash -c '[ -z "$(timeout 2 xsel -b -o 2>/dev/null)" ]'
check "日志含 osc52 store" log_has "osc52 store" "$(app_log "$D1")"
check "日志含 osc52 query" log_has "osc52 query" "$(app_log "$D1")"
stop_app

# ------------------------------------------------- 2) copy-on-select + middle
say "phase 2: copy-on-select / middle-click PRIMARY 回退"
D2=$E2E_OUT/primary
mkdir -p "$D2"
cat > "$D2/pane.sh" <<'EOS'
stty raw -echo
printf '\033[2J\033[H'
printf 'COPYONSELECTPRIMARY\n'
mkfifo "$1/cmd.fifo"
exec 3<>"$1/cmd.fifo"
while IFS= read -r line <&3; do printf '%s' "$line"; done &
cat > "$1/pasted.bin"
EOS
start_app "$D2" allow "$D2/pane.sh" TERVO_MOUSELOG=1 || { echo "FAIL: app 未启动"; exit 1; }
sleep 3
LOG2=$(app_log "$D2")
GEOM=$(window_geom) || { echo "FAIL: 找不到窗口"; exit 1; }
read -r WX WY _WW _WH <<<"$GEOM"
# x=6：避开左侧 resize 热区；y=40：TabBar 32px 后的第一行。
"$XCLICK" $((WX + 6)) $((WY + 40)) drag $((WX + 240)) $((WY + 40)) >/dev/null 2>&1
sleep 1
CLIP=$(timeout 2 xsel -b -o 2>/dev/null)
PRIM=$(timeout 2 xsel -p -o 2>/dev/null)
check "copy-on-select 写剪贴板" [ "$CLIP" = "COPYONSELECTPRIMARY" ]
check "copy-on-select 写 PRIMARY" [ "$PRIM" = "COPYONSELECTPRIMARY" ]

# OSC52 只改普通剪贴板，PRIMARY 仍保留选中文本 → 中键应先粘 PRIMARY。
printf '\033]52;c;Y2xpcGJvYXJkLW9ubHk=\007\n' > "$D2/cmd.fifo"
sleep 0.5
check "OSC52 覆盖普通剪贴板" [ "$(timeout 2 xsel -b -o 2>/dev/null)" = "clipboard-only" ]
"$XCLICK" $((WX + 100)) $((WY + 100)) 2 >/dev/null 2>&1
check "中键优先 PRIMARY" wait_file_tail "$D2/pasted.bin" 1 "COPYONSELECTPRIMARY"
check "日志 middle source=primary" wait_until 3 grep -q "middle paste source=primary" "$LOG2"

# 清空 PRIMARY → 中键回退普通剪贴板。
printf '\033]52;p;\007\n' > "$D2/cmd.fifo"
sleep 0.5
BEFORE=$(wc -c < "$D2/pasted.bin")
"$XCLICK" $((WX + 100)) $((WY + 100)) 2 >/dev/null 2>&1
check "PRIMARY 清空后回退剪贴板" wait_file_tail "$D2/pasted.bin" $((BEFORE + 1)) "clipboard-only"
check "日志 middle source=clipboard" wait_until 3 grep -q "middle paste source=clipboard" "$LOG2"
check "日志 copy-on-select 写 PRIMARY" log_has "PRIMARY 写入" "$LOG2"
stop_app

# ---------------------------------------------------------------- 3) deny
say "phase 3: osc52 = deny"
D3=$E2E_OUT/deny
mkdir -p "$D3"
cat > "$D3/pane.sh" <<'EOS'
stty raw -echo
printf '\033]52;c;ZGVueS10ZXN0\007'
printf '\033]52;p;ZGVueS1wcmltYXJ5\007'
sleep 3
cat > "$1/pasted.bin"
EOS
start_app "$D3" deny "$D3/pane.sh" || { echo "FAIL: app 未启动"; exit 1; }
sleep 3
LOG3=$(app_log "$D3")
check "deny 日志" log_has "osc52 denied" "$LOG3"
if grep -q "osc52 store\|PRIMARY 写入" "$LOG3"; then
  echo "FAIL: deny 下仍处理/写剪贴板"; FAIL=1
else
  echo "PASS: deny 未处理 store、未写 PRIMARY"
fi

echo
if [ "$FAIL" -eq 0 ]; then echo "ALL PASS（日志：$E2E_OUT）"; else echo "有失败项（日志：$E2E_OUT）"; fi
exit "$FAIL"
