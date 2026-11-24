#!/usr/bin/env bash
# GUI e2e 辅助：默认在 headless Xvfb 上跑，不占用 :1，也不产生任何宿主窗口。
# - 每个测试目录一个 headless Xvfb（E2E_DISPLAY，默认 :99）；不创建桌面窗口
# - 只 kill 自己启动的 app/Xephyr
# - 默认使用 master 独立构建 .target-master/release/xtervo

E2E_BIN=${E2E_BIN:-$PWD/.target-master/release/xtervo}
E2E_DISPLAY=${E2E_DISPLAY:-:99}
HOST_DISPLAY=${HOST_DISPLAY:-:1}
E2E_AREA_X=${E2E_AREA_X:-20}
E2E_AREA_Y=${E2E_AREA_Y:-20}
E2E_SCREEN=${E2E_SCREEN:-1280x800x24}
E2E_HOST_X=${E2E_HOST_X:-20}
E2E_HOST_Y=${E2E_HOST_Y:-20}

e2e_display_up() { DISPLAY="$E2E_DISPLAY" xdpyinfo >/dev/null 2>&1; }

e2e_start() {
    local dir=$1; shift
    mkdir -p "$dir"
    if ! e2e_display_up; then
        # 优先系统 Xvfb；本仓库测试环境用 .perf/xvfb 下载版。绝不回退到有宿主窗口的 Xephyr。
        local xvfb="${E2E_XVFB_BIN:-Xvfb}"
        [ -x "$xvfb" ] || xvfb="$PWD/.perf/xvfb/usr/bin/Xvfb"
        if [ ! -x "$xvfb" ]; then
            echo "e2e: 找不到 Xvfb；不会回退到 Xephyr（避免桌面弹窗）。" >&2
            return 1
        fi
        ( "$xvfb" "$E2E_DISPLAY" -screen 0 "$E2E_SCREEN" -ac -nolisten tcp >"$dir/xvfb.log" 2>&1 &
          echo $! >"$dir/xvfb.pid" )
        for _ in $(seq 1 100); do e2e_display_up && break; sleep 0.05; done
        if ! e2e_display_up; then
            echo "e2e: Xvfb $E2E_DISPLAY 启动失败（日志 $dir/xvfb.log）" >&2
            return 1
        fi
    fi
    export DISPLAY="$E2E_DISPLAY"
    ( XMODIFIERS=@im=none XTERVO_LOG_DIR="$dir" XTERVO_CONFIG="$dir/cfg.toml" \
        "$E2E_BIN" "$@" >"$dir/parent.log" 2>&1 & echo $! >"$dir/pid" )
    sleep 2
}

e2e_pid() { cat "$1/pid"; }

e2e_win() {
    local pid; pid=$(e2e_pid "$1")
    for id in $(xwininfo -root -tree 2>/dev/null | grep -iE '"xtervo" "xtervo"' | awk '{print $1}'); do
        local p g
        p=$(xprop -id "$id" _NET_WM_PID 2>/dev/null | grep -o '[0-9]*' | tail -1)
        g=$(xwininfo -id "$id" 2>/dev/null | awk '/Width:/{w=$2}/Height:/{h=$2} END{print w"x"h}')
        [ "$p" = "$pid" ] && echo "$id" && return
    done
}

e2e_move() {  # <win>
    ./target/release/xwin "$1" "$E2E_AREA_X" "$E2E_AREA_Y" >/dev/null 2>&1
    sleep 0.4
}

e2e_pos() {  # <win> -> "x y"
    local win=$1
    local x y
    x=$(xwininfo -id "$win" 2>/dev/null | awk '/Absolute upper-left X/{print $NF}')
    y=$(xwininfo -id "$win" 2>/dev/null | awk '/Absolute upper-left Y/{print $NF}')
    echo "$x $y"
}

e2e_log() { ls -t "$1"/xtervo-*.log 2>/dev/null | head -1; }

e2e_stop() {  # <dir>
    local pid; pid=$(e2e_pid "$1" 2>/dev/null) || true
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
    rm -f "$1/pid"
    # 只有本测试启动的 Xephyr 才回收；外部复用的显示器不动。
    if [ -f "$1/xvfb.pid" ]; then
        kill "$(cat "$1/xvfb.pid")" 2>/dev/null
        rm -f "$1/xvfb.pid"
    fi
    [ -f "$1/xephyr.pid" ] && kill "$(cat "$1/xephyr.pid")" 2>/dev/null && rm -f "$1/xephyr.pid"
}
