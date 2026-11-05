#!/usr/bin/env bash
# 供后续 GUI e2e source 的辅助函数。
# - 固定把测试窗口移到屏幕空白区（避免被用户窗口/旧实例挡住点击）
# - 只 kill 自己启动的 pid，禁止 pkill tervo
# - 默认使用 master 的独立构建 .target-master/release/tervo

E2E_BIN=${E2E_BIN:-$PWD/.target-master/release/tervo}
E2E_AREA_X=${E2E_AREA_X:-3400}
E2E_AREA_Y=${E2E_AREA_Y:-800}

e2e_start() {
    local dir=$1; shift
    mkdir -p "$dir"
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
        [ "$p" = "$pid" ] && [ "$g" = "540x472" ] && { echo "$id"; return; }
    done
}

e2e_move() {  # <win>
    ./target/release/xwin "$1" "$E2E_AREA_X" "$E2E_AREA_Y" >/dev/null 2>&1
    sleep 0.5
}

e2e_pos() {  # <win> -> "x y"
    local win=$1
    local x y
    x=$(xwininfo -id "$win" 2>/dev/null | awk '/Absolute upper-left X/{print $NF}')
    y=$(xwininfo -id "$win" 2>/dev/null | awk '/Absolute upper-left Y/{print $NF}')
    echo "$x $y"
}

e2e_log() {  # <dir> -> newest tervo log
    ls -t "$1"/tervo-*.log 2>/dev/null | head -1
}

e2e_stop() {  # <dir>
    local pid; pid=$(e2e_pid "$1" 2>/dev/null) || return
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
    rm -f "$1/pid"
}
