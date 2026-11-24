#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# W-02 SSH/SFTP/Vault 产品化 GUI 端到端验收（headless Xvfb :139）。
#
# 覆盖：
#   1. SSH Profile 表单：ProxyJump / 远程转发(-R) / 动态转发(-D) / agent forwarding；
#   2. known_hosts 首次连接提示 → Trust & connect（写入 known_hosts 后真实拉起 ssh）；
#   3. SSH 进程退出状态提示 + Reconnect（不自动循环重连）；
#   4. 主机密钥变更提示（旧/新指纹，不静默信任）；
#   5. Vault 后台 Argon2：解锁/初始化期间设置页仍可重绘（busy 截图 + 完成截图），
#      日志不含主密码；
#   6. SFTP 上传/下载队列、Cancel、元数据操作 Cancel、Chmod、失败提示 + Retry。
#
# 用法：cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey
#       ./scripts/e2e_ssh_sftp_vault.sh
# 依赖 Xvfb/Openbox（仓库 .perf/）、xwd、python3+Pillow、xwin/xclick/xkey。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${SSH_SFTP_E2E_OUT:-$ROOT/.logs/ssh-sftp/e2e}
DISPLAY_ID=${E2E_DISPLAY:-:139}
export DISPLAY=$DISPLAY_ID
rm -rf "$OUT"
mkdir -p "$OUT/shots" "$OUT/home/.ssh" "$OUT/logs" "$OUT/bin" /tmp/.X11-unix

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

BIN=${XTERVO_BIN:-$MAIN_ROOT/target/debug/xtervo}
XWIN=${XWIN_BIN:-$MAIN_ROOT/target/debug/xwin}
XCLICK=${XCLICK_BIN:-$MAIN_ROOT/target/debug/xclick}
XKEY=${XKEY_BIN:-$MAIN_ROOT/target/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f（先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey）" >&2; exit 2; }
done

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY 已被占用；先停掉占用它的会话。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
    # 假 ssh/sftp 的 sleep 子进程
    pkill -f '^sleep 300$' 2>/dev/null || true
}
trap cleanup EXIT

"$XVFB_BIN" "$DISPLAY_ID" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
unset XTERVO_SETTINGS_LEGACY

# ---------------------------------------------------------------- 假 ssh/sftp/keyscan
ssh-keygen -q -t ed25519 -N '' -C e2e-old -f "$OUT/key_old"
ssh-keygen -q -t ed25519 -N '' -C e2e-new -f "$OUT/key_new"
KEY_A=$(cat "$OUT/key_old.pub")
KEY_B=$(cat "$OUT/key_new.pub")
cat >"$OUT/known_hosts" <<KH
files.example $KEY_A
KH

cat >"$OUT/bin/ssh-keyscan" <<SH
#!/bin/sh
host=""
for a in "\$@"; do host=\$a; done
sleep 0.35
case "\$host" in
  *files*) printf '%s\\n' "files.example $KEY_B" ;;
  *)       printf '%s\\n' "fresh.example $KEY_A" ;;
esac
SH
chmod +x "$OUT/bin/ssh-keyscan"

# `ssh -G` 交给真实 ssh 解析配置；真正连接时只打印一行并保住 PTY。
cat >"$OUT/bin/ssh" <<'SH'
#!/bin/sh
case " $* " in
  *" -G "*) exec /usr/bin/ssh "$@" ;;
esac
printf 'fake ssh connected\r\n'
exec sleep 300
SH
chmod +x "$OUT/bin/ssh"

# 元数据/list 立即返回（优先 ls -l 带 mode/size/mtime）；传输打印进度后挂起（测试 Cancel）；fail-upload 失败。
cat >"$OUT/bin/sftp" <<'SH'
#!/bin/sh
batch=$(cat)
case "$batch" in
  *"/slow"*) exec sleep 300 ;;
  *"ls -l"*) printf '%s\n' \
      '-rw-r--r--    1 yt yt       128 Nov 20 10:00 alpha.txt' \
      '-rw-r--r--    1 yt yt      2048 Nov 20 10:00 beta.bin' \
      'drwxr-xr-x    2 yt yt      4096 Nov 20 10:00 dir1' ;;
  *"ls -1"*) printf 'alpha.txt\nbeta.bin\ndir1\n' ;;
  *"chmod "*) printf 'chmod ok\n' ;;
  *put*fail*) printf 'permission denied\n' >&2; exit 3 ;;
  *"put "*|*"get "*)
      printf 'alpha.txt 25%% 256KB 1.2MB/s 00:05 ETA\r'
      exec sleep 300 ;;
  *) : ;;
esac
SH
chmod +x "$OUT/bin/sftp"

# ---------------------------------------------------------------- 配置
cat >"$OUT/config.toml" <<'CFG'
version = 1
theme = "tokyo-night"
font_size = 15
scrollback = 2000
confirm_close = false
restore_session_on_startup = false
CFG
cat >"$OUT/profiles.toml" <<PROF
version = 2
next_id = 4

[[profiles]]
id = 1
name = "prod-web"
kind = "ssh"
host = "prod.example"
port = 2222
user = "deploy"
auth = "key"
key_path = "/home/user/.ssh/id_ed25519"
extra_args = ["-F", "/dev/null", "-o", "UserKnownHostsFile=$OUT/known_hosts", "-o", "StrictHostKeyChecking=ask"]
proxy_jump = "ops@bastion:2222"
remote_forwards = ["8080:localhost:80"]
dynamic_forwards = ["1080"]
agent_forward = true

[[profiles]]
id = 2
name = "files"
kind = "ssh"
host = "files.example"
port = 22
user = "deploy"
auth = "key"
key_path = "/home/user/.ssh/id_ed25519"
extra_args = ["-F", "/dev/null", "-o", "UserKnownHostsFile=$OUT/known_hosts", "-o", "StrictHostKeyChecking=ask"]

[[profiles]]
id = 3
name = "fresh"
kind = "ssh"
host = "fresh.example"
port = 22
user = "deploy"
auth = "key"
key_path = "/home/user/.ssh/id_ed25519"
extra_args = ["-F", "/dev/null", "-o", "UserKnownHostsFile=$OUT/known_hosts", "-o", "StrictHostKeyChecking=ask"]
PROF

LOG=$OUT/app.log
PATH="$OUT/bin:$PATH" \
XTERVO_CONFIG="$OUT/config.toml" \
XTERVO_PROFILES="$OUT/profiles.toml" \
XTERVO_VAULT="$OUT/vault.enc" \
XTERVO_LOG_DIR="$OUT/logs" \
XTERVO_VAULT_KDF_DELAY_MS=900 \
    "$BIN" --cols 120 --rows 32 --shell /bin/sh >"$LOG" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || { echo "xtervo 启动失败：$LOG" >&2; exit 2; }
"$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
sleep 1.0

ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
[ "$W" = 1080 ] && [ "$H" = 736 ] || {
    echo "窗口 ${W}x${H} 与坐标假设 1080x736 不符" >&2
    exit 2
}
echo "== xtervo ${W}x${H} at ${ABS_X},${ABS_Y} =="

click() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" >/dev/null; sleep "${3:-0.4}"; }
rclick() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" 3 >/dev/null; sleep "${3:-0.4}"; }
key() { "$XWIN" xtervo >/dev/null 2>&1 || true; "$XKEY" "$@" >/dev/null; }
type_text() { key "$1"; }
type_into() {  # x y text [settle]
    click "$1" "$2" 0.15
    key a --ctrl
    type_text "$3"
    sleep "${4:-0.25}"
}
enter() { key --key enter; }
shot() {
    xwd -silent -id "$WIN" -out "$OUT/shots/$1.xwd"
    python3 "$ROOT/scripts/xwd2png.py" "$OUT/shots/$1.xwd" "$OUT/shots/$1.png" >/dev/null
    rm -f "$OUT/shots/$1.xwd"
    echo "  shot $1"
}
wait_log() {  # pattern timeout
    local pattern=$1 timeout=${2:-5}
    for _ in $(seq 1 $((timeout * 10))); do
        grep -qE "$pattern" "$LOG" && return 0
        sleep 0.1
    done
    echo "等待日志失败: $pattern" >&2
    tail -80 "$LOG" >&2
    exit 2
}
assert_log() { grep -qE "$1" "$LOG" || { echo "日志缺少: $1" >&2; tail -80 "$LOG" >&2; exit 2; }; }
assert_no_log() { ! grep -qF "$1" "$LOG" || { echo "日志不应包含: $1" >&2; exit 2; }; }
assert_file_contains() { grep -qF "$2" "$1" || { echo "$1 缺少: $2" >&2; cat "$1" >&2; exit 2; }; }

open_settings_profiles() {
    click "$((W - 118))" 16 0.8       # 设置齿轮
    click 100 320 0.6                 # Profiles 分类
}
open_settings_vault() {
    click "$((W - 118))" 16 0.8
    click 100 422 0.6                 # Vault 分类
}

echo "== 1. SSH 表单：新字段可编辑/已回填 =="
open_settings_profiles
click 1005 176 0.9                    # 第一行 Edit
shot 01-ssh-form
assert_log '\[profile\] 编辑表单 id=1 name="prod-web" kind=ssh'
# 表单截图里应有 Jump / R-Forwards / D-Forwards / Agent forwarding 与回填值。
python3 - "$OUT/shots/01-ssh-form.png" <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
assert im.size == (1080, 736), im.size
# 表单区域不是纯背景：抽几行像素确认有输入框/文字。
area = im.crop((330, 250, 750, 470))
colors = len(set(area.getdata()))
assert colors > 200, f"SSH 表单区域像素过于单一: {colors}"
print(f"  ssh form pixel colors={colors}")
PY
click 612 682 0.5                     # Cancel

echo "== 2. known_hosts 首次连接：提示 + Trust & connect =="
click 300 244 0.3                     # fresh 行（第三行）
wait_log '预检结果' 6
assert_log 'verdict=unknown.*profile=2'
shot 02-known-hosts-unknown
# 信任按钮：modal 520x470 centered → x=330..502, y=410..438 (panel 起点 280,133)
click 696 557 0.4
wait_log '\[known_hosts\] 已写入' 3
assert_file_contains "$OUT/known_hosts" "fresh.example"
sleep 1.0
shot 03-ssh-connected
assert_log '\[profile\] ssh 命令: ssh'

echo "== 3. SSH 退出状态 + Reconnect =="
pkill -f '^sleep 300$' 2>/dev/null || true
# 退出日志已从 [ssh] ts_ms=... exit= 统一成 [session]（本地 shell/SSH 共用），
# 等价信息仍在：pane=2 + exit 码 + signal。继续断言信号终止，不降级成只看退出码。
wait_log '\[session\] ts_ms=.*pane=2 .*exit=[0-9]+ signal=Some\("Terminated"\)' 6
assert_log '\[connection\] pane=2 state=Failed.*detail="signal Terminated"'
sleep 0.5
shot 04-ssh-exit-banner
click 1002 120 0.5                    # Reconnect
wait_log '\[ssh\] 已重连' 5
shot 05-ssh-reconnected

echo "== 4. known_hosts 变更：旧/新指纹提示，不静默信任 =="
open_settings_profiles
click 300 210 0.3                     # files 行（known_hosts 记录 KEY_A，keyscan 返回 KEY_B）
wait_log 'verdict=changed.*profile=1' 6
sleep 0.5
shot 06-known-hosts-changed
# Cancel：不写 known_hosts，旧条目保持不变
click 373 557 0.4
python3 - "$OUT/known_hosts" <<'PY'
import sys
text = open(sys.argv[1]).read()
assert "files.example ssh-ed25519" in text, text
PY

echo "== 5. Vault：Argon2 在后台，解锁期间设置页可重绘 =="
# 上一步 Cancel 后设置页仍开着；直接切 Vault 分类即可。
click 100 422 0.6
shot 07-vault-locked
type_into 650 248 "e2emasterpass" 0.2
click 366 287 0.15                    # Set / Change（初始化）
shot 08-vault-init-busy
wait_log '后台 KDF 开始' 3
wait_log '后台初始化完成' 5
sleep 0.3
shot 09-vault-initialized
click 797 287 0.4                     # Lock
wait_log '\[vault\] 已锁定' 2
type_into 650 248 "e2emasterpass" 0.2
click 582 287 0.15                    # Unlock
shot 10-vault-unlock-busy
wait_log '后台解锁完成' 5
sleep 0.3
shot 11-vault-unlocked
assert_log '后台 KDF 开始'
assert_no_log 'e2emasterpass'
[ -s "$OUT/vault.enc" ] || { echo "vault.enc 未生成" >&2; exit 2; }
python3 - "$OUT/vault.enc" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
assert b"e2emasterpass" not in data, "vault 文件泄露主密码"
assert b"XTERVO-VAULT 1" in data
print(f"  vault file ok bytes={len(data)}")
PY

echo "== 6. SFTP 队列 / 取消 / 元数据取消 / chmod / retry =="
# 先关掉覆盖全窗口的 Settings Tab，再从标签栏右键菜单打开 SFTP。
key --key escape
sleep 0.5
rclick 500 16 0.4
shot 12a-tabbar-menu
click 585 320 0.8                     # 菜单 SFTP（第 10 项）
wait_log '\[sftp\] 打开面板' 3
click 504 113 0.4                     # 选择 files Profile
click 356 184 0.9                     # List
shot 12-sftp-list
click 400 217 0.3                     # 选中 alpha.txt
# 本地目标用不存在的路径，避免“目标已存在”确认影响队列测试（覆盖确认另有单测）。
DOWNLOAD_TARGET="$OUT/download-target.bin"
rm -f "$DOWNLOAD_TARGET"
type_into 500 408 "$DOWNLOAD_TARGET" 0.2
click 390 473 0.4                     # Download → 开始
click 390 473 0.5                     # Download again → 入队
shot 13-sftp-queue
wait_log '传输已入队 queue=1' 3
click 540 505 0.3                     # Clear queue（避免取消后自动开始下一项）
click 712 615 0.6                     # Cancel 当前传输
wait_log '传输结束 phase=Cancelled' 6
shot 14-sftp-cancelled

# 元数据操作取消：/slow 让假 sftp 挂起，点 Cancel 必须 kill 并回状态。
type_into 500 148 "/slow" 0.2
click 356 184 0.5                     # List /slow
sleep 0.2
click 712 615 0.3                     # Cancel 元数据操作
wait_log '后台操作完成: list /slow 失败: sftp 操作已取消' 6
shot 15-sftp-op-cancelled

# 恢复列表、选中首项、chmod 640
type_into 500 148 "/" 0.2
click 356 184 0.8
click 400 217 0.3
type_into 416 440 "640" 0.2
click 585 439 1.0
wait_log 'chmod 0640 /alpha.txt' 5
shot 16-sftp-chmod

# 失败提示 + Retry
type_into 500 408 "/tmp/fail-upload.txt" 0.2
click 512 473 1.2                     # Upload → 假 sftp 返回 permission denied
wait_log '传输结束 phase=Failed' 5
shot 17-sftp-failed
click 595 473 1.2                     # Retry（仍失败，但按钮/状态机可重入）
wait_log '传输结束 phase=Failed' 5
shot 18-sftp-retry

echo "== 7. 截图与日志检查 =="
python3 - "$OUT/shots" <<'PY'
import os, sys
from PIL import Image
d = sys.argv[1]
shots = sorted(f for f in os.listdir(d) if f.endswith(".png"))
assert len(shots) >= 16, shots
for f in shots:
    im = Image.open(os.path.join(d, f)).convert("RGB")
    assert im.size == (1080, 736), (f, im.size)
    assert len(set(im.getdata())) > 100, f"截图疑似空白: {f}"
print(f"  {len(shots)} screenshots ok")
PY
assert_log '\[known_hosts\] 预检结果: 无法预检|\[known_hosts\] 预检结果'
assert_log '\[ssh\] 已重连'
assert_log '\[session\] ts_ms=.* pane=2 .*exit=[0-9]+ signal=Some\("Terminated"\)'   # 记录退出码/信号
echo "全部通过；截图在 $OUT/shots"

# 保留日志/截图供复盘；cleanup 由 trap 回收 Xvfb/app。
