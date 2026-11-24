// SPDX-License-Identifier: GPL-3.0-only
//! 输出过滤器热路径契约：PTY bytes 在 vte 之前被替换；任何失败都原样放行。
//!
//! 这些测试使用真实 PTY + shell，不 mock actor；WASM 侧的同类 e2e 在
//! `crates/app/src/plugin_v3/tests.rs`（真实 Wasmtime 实例 + 真实 PTY）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use xtervo_core::{
    spawn, Frame, OutputFilter, OutputFilterHandle, PaneConfig, PaneHandle, MAX_OUTPUT_FILTER_BYTES,
};

struct Replace {
    from: &'static [u8],
    to: &'static [u8],
}

impl OutputFilter for Replace {
    fn filter(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i..].starts_with(self.from) {
                out.extend_from_slice(self.to);
                i += self.from.len();
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Some(out)
    }
}

struct AlwaysNone;
impl OutputFilter for AlwaysNone {
    fn filter(&self, _bytes: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

struct Panics;
impl OutputFilter for Panics {
    fn filter(&self, _bytes: &[u8]) -> Option<Vec<u8>> {
        panic!("test filter panic must not crash xtervo");
    }
}

struct Oversize;
impl OutputFilter for Oversize {
    fn filter(&self, _bytes: &[u8]) -> Option<Vec<u8>> {
        Some(vec![b'x'; MAX_OUTPUT_FILTER_BYTES + 1])
    }
}

fn sh(script: &str, filter: Arc<dyn OutputFilter>) -> PaneConfig {
    PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 200,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        output_filter: Some(OutputFilterHandle::new(filter)),
        ..PaneConfig::default()
    }
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|cell| cell.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_contains(
    handle: &PaneHandle,
    mut frame: Box<Frame>,
    needle: &str,
    timeout: Duration,
) -> (Box<Frame>, String) {
    let deadline = Instant::now() + timeout;
    loop {
        frame = handle.pull(frame);
        let text = frame_text(&frame);
        if text.contains(needle) {
            return (frame, text);
        }
        if Instant::now() > deadline {
            panic!("等待 {needle:?} 超时；当前屏幕:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn filter_replaces_bytes_before_vte() {
    let handle = spawn(sh(
        "printf 'hello FILTER-ME\\n'; sleep 0.2",
        Arc::new(Replace {
            from: b"FILTER-ME",
            to: b"FILTERED",
        }),
    ))
    .expect("spawn");
    let frame = handle.new_frame();
    let (_, text) = wait_contains(&handle, frame, "hello FILTERED", Duration::from_secs(5));
    assert!(
        !text.contains("FILTER-ME"),
        "过滤后的输出不应再出现原 pattern:\n{text}"
    );
}

#[test]
fn filter_none_falls_back_to_exact_original_bytes() {
    let handle = spawn(sh(
        "printf 'fallback-none \x1b[31mRED\x1b[0m\\n'; sleep 0.2",
        Arc::new(AlwaysNone),
    ))
    .expect("spawn");
    let frame = handle.new_frame();
    let (_, text) = wait_contains(&handle, frame, "fallback-none", Duration::from_secs(5));
    assert!(text.contains("fallback-none"), "{text}");
    assert!(text.contains("RED"), "ANSI 颜色文字必须保留: {text}");
}

#[test]
fn filter_panic_is_caught_and_original_passes_through() {
    let handle = spawn(sh(
        "printf 'panic-fallback-ok\\n'; sleep 0.2",
        Arc::new(Panics),
    ))
    .expect("spawn");
    let frame = handle.new_frame();
    let (_, text) = wait_contains(&handle, frame, "panic-fallback-ok", Duration::from_secs(5));
    assert!(text.contains("panic-fallback-ok"), "{text}");
}

#[test]
fn oversize_filter_output_is_rejected_and_original_passes_through() {
    let handle = spawn(sh(
        "printf 'oversize-fallback-ok\\n'; sleep 0.2",
        Arc::new(Oversize),
    ))
    .expect("spawn");
    let frame = handle.new_frame();
    let (_, text) = wait_contains(
        &handle,
        frame,
        "oversize-fallback-ok",
        Duration::from_secs(5),
    );
    assert!(text.contains("oversize-fallback-ok"), "{text}");
}
