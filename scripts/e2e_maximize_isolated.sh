#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 隔离最大化验收：独立 Xvfb + Openbox + 独立 HOME；拒绝 :1。
# 启动一个 xtervo 实例，点击最大化/恢复，断言 EWMH 状态和几何改变。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DISPLAY_NUM=${DISPLAY_NUM:-:97}
if [ "$DISPLAY_NUM" = ":1" ]; then
    echo "ERROR: 不允许在用户 :1 上测试" >&2
    exit 2
fi
BIN=${BIN:-$ROOT/target/release/xtervo}
XVFB=${XVFB:-$ROOT/.perf/xvfb/usr/bin/Xvfb}
OPENBOX=${OPENBOX:-$ROOT/.perf/openbox/usr/bin/openbox}
LIB=$ROOT/.perf/openbox/usr/lib/x86_64-linux-gnu
DATA=$ROOT/.perf/openbox/usr/share
OUT=${SOAK_OUT:-$ROOT/.logs/maximize-isolated}
rm -rf "$OUT"; mkdir -p "$OUT/home/.config/openbox"
cat >"$OUT/home/.config/openbox/rc.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<openbox_config xmlns="http://openbox.org/3.4/rc"><theme><name>Clearlooks</name><titleLayout>NLIMC</titleLayout></theme><desktops><number>1</number></desktops><resize><drawContents>yes</drawContents></resize><keyboard></keyboard><mouse></mouse></openbox_config>
XML
PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
}
trap cleanup EXIT

"$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
PIDS+=($!)
sleep 0.7
DISPLAY="$DISPLAY_NUM" HOME="$OUT/home" XDG_DATA_DIRS="$DATA:/usr/share" \
    LD_LIBRARY_PATH="$LIB" "$OPENBOX" --sm-disable >"$OUT/openbox.log" 2>&1 &
PIDS+=($!)
sleep 1

DISPLAY="$DISPLAY_NUM" HOME="$OUT/home" "$BIN" --title "xtervoIso" \
    --cols 60 --rows 20 --shell /bin/bash -- -c 'sleep 30' \
    >"$OUT/app.log" 2>&1 &
PIDS+=($!)
sleep 2

DISPLAY="$DISPLAY_NUM" OUT="$OUT" python3 - <<'PY'
import ctypes, os, subprocess, time, sys
disp=os.environ['DISPLAY']; out=os.environ['OUT']
x11=ctypes.CDLL('libX11.so.6'); xtst=ctypes.CDLL('libXtst.so.6')
x11.XOpenDisplay.restype=ctypes.c_void_p
d=x11.XOpenDisplay(disp.encode()); assert d, 'XOpenDisplay failed'
x11.XRaiseWindow.argtypes=[ctypes.c_void_p,ctypes.c_ulong]
x11.XSetInputFocus.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.c_int,ctypes.c_ulong]
xtst.XTestFakeMotionEvent.argtypes=[ctypes.c_void_p,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_ulong]
xtst.XTestFakeButtonEvent.argtypes=[ctypes.c_void_p,ctypes.c_uint,ctypes.c_int,ctypes.c_ulong]
x11.XFlush.argtypes=[ctypes.c_void_p]
def shell(*a):
    return subprocess.run(a, capture_output=True, text=True, check=True).stdout
def geometry(wid):
    g=shell('xwininfo','-display',disp,'-id',wid)
    v={}
    for l in g.splitlines():
        s=l.strip()
        for k,p in [('x','Absolute upper-left X:'),('y','Absolute upper-left Y:'),('w','Width:'),('h','Height:')]:
            if s.startswith(p): v[k]=int(s.split(':')[1])
    return v
def state(wid):
    return shell('xprop','-display',disp,'-id',wid,'_NET_WM_STATE').strip()
def click(wid):
    g=geometry(wid)
    # 按钮命中区起点 width-70，宽 40；中心 width-50，y=2+15。
    cx, cy = g['x']+g['w']-50, g['y']+17
    xtst.XTestFakeMotionEvent(d,0,cx,cy,0); x11.XFlush(d); time.sleep(.15)
    xtst.XTestFakeButtonEvent(d,1,1,0); x11.XFlush(d); time.sleep(.05)
    xtst.XTestFakeButtonEvent(d,1,0,0); x11.XFlush(d); time.sleep(.9)
    return g
# find windows by title (exact WM_NAME)
tree=shell('xwininfo','-display',disp,'-root','-tree')
ids=[]
for l in tree.splitlines():
    if '"xtervoIso"' in l:
        ids.append(('xtervoIso',l.split()[0])); break
print('windows', ids)
assert len(ids)==1, ids
for title,wid in ids:
    x11.XRaiseWindow(d,int(wid,16)); x11.XSetInputFocus(d,int(wid,16),1,0); x11.XFlush(d); time.sleep(.3)
    before=geometry(wid); click(wid); maxed=geometry(wid); st_max=state(wid)
    print(title,'max',before,'->',maxed, st_max)
    assert 'MAXIMIZED_HORZ' in st_max and 'MAXIMIZED_VERT' in st_max, (title,st_max)
    # 锁死双屏回归：_NET_WORKAREA 是整块虚拟桌面（例如 5120x1413），
    # 单屏最大化窗口只有 2560x1440。伪造成 2560x800，旧几何校准会误判 false。
    shell('xprop','-display',disp,'-root','-f','_NET_WORKAREA','32c',
          '-set','_NET_WORKAREA','0,0,2560,800')
    click(wid); restored=geometry(wid); st_rest=state(wid)
    print(title,'restore ->',restored, st_rest)
    assert 'MAXIMIZED_HORZ' not in st_rest and 'MAXIMIZED_VERT' not in st_rest, (title,st_rest)
print('PASS maximize isolated: maximize + restore')
PY
