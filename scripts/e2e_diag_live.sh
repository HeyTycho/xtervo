#!/usr/bin/env bash
# 只读 live diag socket + MCP live tools 验收。
#
# 覆盖：
#   - XTERVO_DIAG_SOCKET 创建 0600 Unix socket，未授权请求被拒绝
#   - capabilities / snapshot 经 UI 线程取真实 pane frame/cursor
#   - `xtervo diag serve --socket --token` 暴露 live_capabilities/live_snapshot
#   - MCP tools/list 同时包含静态 env/snapshot/replay 和 live 工具
#
# 只使用 headless Xvfb，只 kill 本脚本启动的进程；不碰 :1、不 pkill。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/diag-live-e2e}
DISPLAY_NUM=${E2E_DISPLAY:-:212}
SCREEN=${E2E_SCREEN:-1280x800x24}
BIN=${E2E_BIN:-$ROOT/target/debug/xtervo}
XVFB=${E2E_XVFB_BIN:-$ROOT/.perf/xvfb/usr/bin/Xvfb}
[ -x "$BIN" ] || { echo "缺少 $BIN；先 cargo build -p xtervo-app --bin xtervo" >&2; exit 2; }
[ -x "$XVFB" ] || { echo "缺少 Xvfb: $XVFB" >&2; exit 2; }

if [ -n "${E2E_OUT:-}" ] && [ -e "$OUT" ]; then
    echo "E2E_OUT=$OUT 已存在；拒绝删除调用者目录。请换一个新目录或先手动清理。" >&2
    exit 2
fi
rm -rf -- "$OUT"; mkdir -p "$OUT/home"
SOCK="$OUT/diag.sock"; TOKEN=test-token-$RANDOM
APP_PID=""; XVFB_PID=""
cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
  rm -f "$SOCK"
}
trap cleanup EXIT
if DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1; then
  echo "FAIL: display $DISPLAY_NUM 已被占用" >&2; exit 1
fi
"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done

env DISPLAY=$DISPLAY_NUM HOME="$OUT/home" XTERVO_CONFIG="$OUT/config.toml" \
  XTERVO_LOG_DIR="$OUT/logs" XTERVO_DIAG_SOCKET="$SOCK" XTERVO_DIAG_TOKEN="$TOKEN" \
  "$BIN" --shell /bin/bash --cols 80 --rows 20 --title xtervo >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 100); do [ -S "$SOCK" ] && break; sleep 0.05; done
[ -S "$SOCK" ] || { echo "FAIL: diag socket 未创建"; cat "$OUT/app.log"; exit 1; }
MODE=$(stat -c '%a' "$SOCK")
[ "$MODE" = 600 ] || { echo "FAIL: socket 权限 $MODE != 600"; exit 1; }

python3 - "$SOCK" "$TOKEN" <<'PY'
import json, socket, sys
sock, token = sys.argv[1], sys.argv[2]
def call(obj):
    s = socket.socket(socket.AF_UNIX); s.settimeout(4); s.connect(sock)
    s.sendall((json.dumps(obj)+'\n').encode())
    chunks = []
    while True:
        data = s.recv(65536)
        if not data:
            break
        chunks.append(data)
        if b'\n' in data:
            break
    s.close()
    return json.loads(b''.join(chunks))
r = call({'id': 1, 'cmd': 'capabilities'})
assert r['ok'] is False and r['error'] == 'unauthorized', r
r = call({'id': 2, 'cmd': 'capabilities', 'token': token})
assert r['ok'] is True and r['data']['read_only'] is True, r
r = call({'id': 3, 'cmd': 'snapshot', 'token': token})
assert r['ok'] is True and r['data']['kind'] == 'live', r
panes = r['data']['panes']; assert panes, r
p = panes[0]
assert p['cols'] == 80 and p['rows'] == 20, p
assert isinstance(p.get('text'), list) and p['text'], p
assert 'cursor' in p and 'images' in p, p
print('PASS: live socket capabilities/snapshot; pane=%s text0=%r' % (p['pane_id'], p['text'][0]))
PY

printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"xtervo.live_snapshot","arguments":{}}}' \
  | "$BIN" diag serve --socket "$SOCK" --token "$TOKEN" >"$OUT/mcp.jsonl" 2>"$OUT/mcp.err"
python3 - "$OUT/mcp.jsonl" <<'PY'
import json, sys
msgs = [json.loads(l) for l in open(sys.argv[1])]
tools = None; live = None
for m in msgs:
    if m.get('id') == 2: tools = [t['name'] for t in m['result']['tools']]
    if m.get('id') == 3: live = m['result']
for want in ('xtervo.diag_env','xtervo.diag_snapshot','xtervo.diag_replay','xtervo.live_snapshot'):
    assert tools and want in tools, (want, tools)
assert live and live['isError'] is False, live
data = json.loads(live['content'][0]['text'])
assert data['kind'] == 'live' and data['panes'], data
print('PASS: MCP tools/list + live_snapshot; panes=%d' % len(data['panes']))
PY

echo "PASS: diag live e2e; log=$OUT/app.log"
