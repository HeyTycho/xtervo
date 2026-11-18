// SPDX-License-Identifier: GPL-3.0-only
//! 伪 TUI 端到端：应用在 PTY 里发 query/set/push/pop/modifyOtherKeys，
//! core 必须把正确应答写回 PTY；关闭协议时一个字节都不能多。
//!
//! 断言方式与 `osc52.rs` 一致：子进程用 `dd` 把 PTY 输入落盘，测试读文件；
//! 不依赖屏幕解析，也不伪造数据。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tervo_core::{spawn, PaneConfig, PaneHandle};

fn sh(script: &str, keyboard_protocol: bool) -> PaneConfig {
    PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 500,
        keyboard_protocol,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    }
}

fn out_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tervo-kbd-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_bytes(path: &Path, want: usize, timeout: Duration) -> Vec<u8> {
    let start = Instant::now();
    loop {
        let data = std::fs::read(path).unwrap_or_default();
        if data.len() >= want {
            return data;
        }
        if start.elapsed() > timeout {
            panic!(
                "等待 {} 的 {want} 字节超时，当前 {:?}",
                path.display(),
                String::from_utf8_lossy(&data)
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_level(handle: &PaneHandle, want: u8, timeout: Duration) {
    let start = Instant::now();
    while handle.modify_other_keys() != want {
        if start.elapsed() > timeout {
            panic!(
                "modifyOtherKeys 等级未到 {want}，当前 {}",
                handle.modify_other_keys()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn dd_cmd(path: &Path, count: usize) -> String {
    format!(
        "timeout --foreground 3 dd bs=1 count={count} of={} 2>/dev/null",
        path.display()
    )
}

#[test]
fn kitty_query_gets_reply_from_pseudo_tui() {
    let dir = out_dir("query");
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '\\033[?u'; {}",
        dd_cmd(&out, 5)
    );
    let _handle = spawn(sh(&script, true)).expect("spawn pane");
    let data = wait_bytes(&out, 5, Duration::from_secs(5));
    assert_eq!(data, b"\x1b[?0u", "初始 flags 必须是 0");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kitty_set_push_pop_query_roundtrip() {
    let dir = out_dir("stack");
    let f1 = dir.join("set.bin");
    let f2 = dir.join("push.bin");
    let f3 = dir.join("pop.bin");
    // set 3（不 push）→ query；push 7 → query；pop → query。
    // 这是 alacritty 0.26 会答错的 set-without-push 与 set-push-pop 场景。
    let script = format!(
        "stty raw -echo; \
         printf '\\033[=3;1u\\033[?u'; {dd1}; \
         printf '\\033[>7u\\033[?u'; {dd2}; \
         printf '\\033[<1u\\033[?u'; {dd3}; sleep 0.2",
        dd1 = dd_cmd(&f1, 5),
        dd2 = dd_cmd(&f2, 5),
        dd3 = dd_cmd(&f3, 5),
    );
    let handle = spawn(sh(&script, true)).expect("spawn pane");
    assert_eq!(wait_bytes(&f1, 5, Duration::from_secs(5)), b"\x1b[?3u");
    assert_eq!(wait_bytes(&f2, 5, Duration::from_secs(5)), b"\x1b[?7u");
    assert_eq!(wait_bytes(&f3, 5, Duration::from_secs(5)), b"\x1b[?3u");
    // pop 后 UI 快照也必须是恢复后的 3。
    let start = Instant::now();
    while handle.keyboard_flags().bits() != 3 {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("flags 快照未恢复为 3，当前 {}", handle.keyboard_flags().bits());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kitty_alt_screen_has_independent_flags() {
    let dir = out_dir("alt");
    let f1 = dir.join("alt.bin");
    let f2 = dir.join("main.bin");
    let script = format!(
        "stty raw -echo; \
         printf '\\033[=1;1u\\033[?1049h\\033[=3;1u\\033[?u'; {dd1}; \
         printf '\\033[?1049l\\033[?u'; {dd2}; sleep 0.2",
        dd1 = dd_cmd(&f1, 5),
        dd2 = dd_cmd(&f2, 5),
    );
    let _handle = spawn(sh(&script, true)).expect("spawn pane");
    assert_eq!(
        wait_bytes(&f1, 5, Duration::from_secs(5)),
        b"\x1b[?3u",
        "备用屏自己的 set 应生效"
    );
    assert_eq!(
        wait_bytes(&f2, 5, Duration::from_secs(5)),
        b"\x1b[?1u",
        "离开备用屏后必须恢复主屏 flags"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn modify_other_keys_level_and_query_reach_pseudo_tui() {
    let dir = out_dir("modify");
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '\\033[>4;2m\\033[?4m'; {}",
        dd_cmd(&out, 7)
    );
    let handle = spawn(sh(&script, true)).expect("spawn pane");
    assert_eq!(
        wait_bytes(&out, 7, Duration::from_secs(5)),
        b"\x1b[>4;2m",
        "查询必须回当前 level"
    );
    wait_level(&handle, 2, Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn modify_other_keys_without_value_enables_level_two() {
    // 兼容性选择：`CSI > 4 m`（无 Pv）按 level 2 启用；显式 `; 0` 才关闭。
    let dir = out_dir("modify-default");
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; printf '\\033[>4m\\033[?4m'; {}",
        dd_cmd(&out, 7)
    );
    let handle = spawn(sh(&script, true)).expect("spawn pane");
    assert_eq!(wait_bytes(&out, 7, Duration::from_secs(5)), b"\x1b[>4;2m");
    wait_level(&handle, 2, Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn protocol_off_answers_nothing() {
    let dir = out_dir("off");
    let out = dir.join("reply.bin");
    let done = dir.join("done");
    // query + modifyOtherKeys 都不应得到回应；1s 超时后 dd 退出并写 marker。
    let script = format!(
        "stty raw -echo; printf '\\033[?u\\033[>4;2m\\033[?4m'; \
         timeout --foreground 1 dd bs=1 count=1 of={} 2>/dev/null; \
         printf done > {}",
        out.display(),
        done.display()
    );
    let handle = spawn(sh(&script, false)).expect("spawn pane");
    let start = Instant::now();
    while !done.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("子进程未在超时内结束");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let data = std::fs::read(&out).unwrap_or_default();
    assert!(data.is_empty(), "keyboard_protocol=off 仍回了字节: {data:?}");
    assert_eq!(handle.keyboard_flags().bits(), 0);
    assert_eq!(handle.modify_other_keys(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 应用崩溃/被 kill 时不会发 `CSI < u`。子命令退出、shell 回到前台后，
/// 必须复位残留的 kitty flags，否则 shell 会把按键回显成 `9;5:3u` 这类乱码。
#[test]
fn foreground_command_exit_resets_stale_kitty_flags() {
    let handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 200,
        keyboard_protocol: true,
        shell: Some("/bin/sh".to_string()),
        ..PaneConfig::default()
    })
    .expect("spawn interactive shell");

    // 等交互 shell 起来并打印首个 prompt。
    std::thread::sleep(Duration::from_millis(300));

    // 子进程设置 flags=7 后退出；shell 随后回到前台。
    handle.input(b"sh -c 'printf \"\\033[>7u\"; sleep 0.3'\n");

    let start = Instant::now();
    while handle.keyboard_flags().bits() != 7 {
        if start.elapsed() > Duration::from_secs(5) {
            panic!(
                "未观察到子进程设置的 flags=7，当前 {}",
                handle.keyboard_flags().bits()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let start = Instant::now();
    while handle.keyboard_flags().bits() != 0 {
        if start.elapsed() > Duration::from_secs(5) {
            panic!(
                "前台命令退出后 flags 未复位为 0，当前 {}",
                handle.keyboard_flags().bits()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
