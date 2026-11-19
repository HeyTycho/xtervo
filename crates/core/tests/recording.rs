// SPDX-License-Identifier: GPL-3.0-only
//! TERM-015：pane 级会话录制 API（真实 PTY + shell）。
//!
//! 验证的是 UI 只需要调用的最小契约：开始 → core 累积 PTY **原始字节**
//! （含 SGR 转义）→ 停止 → 导出文件逐字节等于录制快照。

use std::time::{Duration, Instant};

use tervo_core::{spawn, PaneConfig};

#[test]
fn pane_recording_captures_raw_pty_bytes_and_exports() {
    let handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            "sleep 0.4; printf 'REC-START\\033[31mRED\\033[0m\\n'; sleep 0.3".to_string(),
        ],
        ..PaneConfig::default()
    })
    .expect("spawn");

    // shell 先睡 0.4s：这里开始录制能稳定覆盖到下面那行输出。
    let started = handle.start_recording();
    assert!(started.active);
    assert_eq!(started.bytes, 0);
    assert!(started.started_at.is_some());

    let deadline = Instant::now() + Duration::from_secs(5);
    let raw = loop {
        let bytes = handle.recording_snapshot();
        if bytes
            .windows(b"REC-START\x1b[31mRED\x1b[0m\r\n".len())
            .any(|w| w == b"REC-START\x1b[31mRED\x1b[0m\r\n")
        {
            break bytes;
        }
        assert!(
            Instant::now() < deadline,
            "等待录制内容超时；已录 {} 字节: {:?}",
            bytes.len(),
            String::from_utf8_lossy(&bytes)
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    // 原始字节：必须带 SGR 序列，而不是渲染后的纯文本。
    assert!(
        raw.windows(b"REC-START\x1b[31mRED\x1b[0m".len())
            .any(|w| w == b"REC-START\x1b[31mRED\x1b[0m"),
        "录制应为 PTY 原始字节: {:?}",
        String::from_utf8_lossy(&raw)
    );

    let stopped = handle.stop_recording();
    assert!(!stopped.active);
    assert!(stopped.bytes >= raw.len());
    assert!(stopped.stopped_at.is_some());
    // 停止后快照保留，允许“先停止、后导出”。
    assert_eq!(handle.recording_status().bytes, stopped.bytes);

    let path = std::env::temp_dir().join(format!(
        "tervo-recording-e2e-{}-{}.raw",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let exported = handle.export_recording(&path).expect("export recording");
    assert_eq!(exported.bytes, stopped.bytes);
    assert!(!exported.active);
    let written = std::fs::read(&path).expect("read export");
    assert_eq!(written, handle.recording_snapshot());
    let _ = std::fs::remove_file(&path);

    // 录制关闭后新输出不再写入；再次 start 会清空上一段。
    let restarted = handle.start_recording();
    assert!(restarted.active);
    assert_eq!(restarted.bytes, 0);
    handle.stop_recording();
}

#[test]
fn recording_stops_when_pty_reaches_eof() {
    use tervo_core::CoreEvent;
    let handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "sleep 0.2; exit 0".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");
    assert!(handle.start_recording().active);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut events = Vec::new();
        handle.drain_events(&mut events);
        if events.iter().any(|e| matches!(e, CoreEvent::Exited)) {
            break;
        }
        assert!(Instant::now() < deadline, "timeout waiting for Exited");
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = handle.recording_status();
    assert!(!status.active, "recorder must stop at PTY EOF: {status:?}");
    assert!(status.stopped_at.is_some());
}
