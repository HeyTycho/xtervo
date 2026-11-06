#!/usr/bin/env bash
# GUI e2e 辅助：默认在 Xephyr 虚拟显示器上跑，绝不占用用户的 :1 键鼠。
# - 每个测试目录一个 Xephyr（E2E_DISPLAY，默认 :99）
# - 只 kill 自己启动的 app/Xephyr
# - 默认使用 master 独立构建 .target-master/release/tervo

E2E_BIN=${E2E_BIN:-$PWD/.target-master/release/tervo}
E2E_DISPLAY=${E2E_DISPLAY:-:99}
HOST_DISPLAY=${HOST_DISPLAY:-:1}
E2E_AREA_X=${E2E_AREA_X:-20}
E2E_AREA_Y=${E2E_AREA_Y:-20}
E2E_SCREEN=${E2E_SCREEN:-1280x800x24}

e2e_display_up() { DISPLAY="$E2E_DISPLAY" xdpyinfo >/dev/null 2>&1; }

e2e_start() {
    local dir=$1; shift
    mkdir -p "$dir"
    if ! e2e_display_up; then
        ( DISPLAY="$HOST_DISPLAY" Xephyr "$E2E_DISPLAY" \
            -screen "$E2E_SCREEN" -ac -br -noreset -no-host-grab \
            -title "Tervo E2E $E2E_DISPLAY" >"$dir/xephyr.log" 2>&1 &
          echo $! >"$dir/xephyr.pid" )
        for _ in $(seq 1 100); do e2e_display_up && break; sleep 0.05; done
        if ! e2e_display_up; then
            echo "e2e: Xephyr $E2E_DISPLAY 启动失败（日志 $dir/xephyr.log）" >&2
            return 1
        fi
    fi
    export DISPLAY="$E2E_DISPLAY"
    ( XMODIFIERS=@im=none TERVO_LOG_DIR="$dir" TERVO_CONFIG="$dir/cfg.toml" \
        "$E2E_BIN" "$@" >"$dir/parent.log" 2>&1 & echo $! >"$dir/pid" )
    sleep 2
}

e2e_pid() { cat "$1/pid"; }

e2e_win() {
    local pid; pid=$(e2e_pid "$1")
    for id in $(xwininfo -root -tree 2>/dev/null | grep -iE '"tervo" "tervo"' | awk '{print $1}'); do
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

e2e_log() { ls -t "$1"/tervo-*.log 2>/dev/null | head -1; }

e2e_stop() {  # <dir>
    local pid; pid=$(e2e_pid "$1" 2>/dev/null) || true
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
    rm -f "$1/pid"
    # 只有本测试启动的 Xephyr 才回收；外部复用的显示器不动。
    if [ -f "$1/xephyr.pid" ]; then
        kill "$(cat "$1/xephyr.pid")" 2>/dev/null
        rm -f "$1/xephyr.pid"
    fi
}
