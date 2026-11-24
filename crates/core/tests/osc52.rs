// SPDX-License-Identifier: GPL-3.0-only
//! OSC 52 端到端：PTY 输出 → core 事件；core 查询回写 → PTY 输入。

use std::time::{Duration, Instant};

use xtervo_core::{spawn, ClipboardRequest, ClipboardSelection, CoreEvent, PaneConfig, PaneHandle};

fn sh(script: &str) -> PaneConfig {
    PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 500,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    }
}

fn wait_event(
    handle: &PaneHandle,
    timeout: Duration,
    pred: impl Fn(&CoreEvent) -> bool,
) -> CoreEvent {
    let start = Instant::now();
    let mut events = Vec::new();
    loop {
        handle.drain_events(&mut events);
        if let Some(pos) = events.iter().position(&pred) {
            return events.swap_remove(pos);
        }
        if start.elapsed() > timeout {
            panic!("超时等待 CoreEvent，已收到 {events:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn osc52_store_arrives_as_core_event() {
    let handle = spawn(sh("printf '\\033]52;c;aGVsbG8=\\007'; sleep 1")).expect("spawn pane");
    let ev = wait_event(&handle, Duration::from_secs(5), |e| {
        matches!(e, CoreEvent::Clipboard(_))
    });
    assert_eq!(
        ev,
        CoreEvent::Clipboard(ClipboardRequest::Store {
            selection: ClipboardSelection::Clipboard,
            text: "hello".to_string(),
        })
    );
}

#[test]
fn clipboard_reply_reaches_child_pty() {
    let dir = std::env::temp_dir().join(format!("xtervo-osc52-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("reply.bin");
    let script = format!(
        "stty raw -echo; timeout --foreground 3 dd bs=1 of={} 2>/dev/null",
        out.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    // 等 shell 进入 raw + dd；PTY 输入会排队，早一点写也不会丢。
    std::thread::sleep(Duration::from_millis(500));
    handle.input(b"PING");
    handle.clipboard_reply(ClipboardSelection::Clipboard, "hello");

    let want = b"PING\x1b]52;c;aGVsbG8=\x1b\\";
    let start = Instant::now();
    loop {
        if std::fs::read(&out).map(|b| b == want).unwrap_or(false) {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let got = std::fs::read(&out).unwrap_or_default();
            panic!("查询回写未到达 PTY，收到 {got:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
