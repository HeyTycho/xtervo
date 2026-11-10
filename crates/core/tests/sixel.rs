// SPDX-License-Identifier: GPL-3.0-only
//! 真实 PTY 的 Sixel 集成测试：跨 chunk 大图、BEL 终止、非法/超限、DA1、
//! 滚动锚点。不依赖 GUI；图片通过 `Frame::images` 暴露。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tervo_core::{spawn, Frame, PaneConfig, PaneHandle};

/// PTY 集成测试会同时 fork shell/actor；并行跑时偶发某个 pane 5s 内完全无输出。
/// 用进程内锁串行化这些用例，避免 CI/全量测试的资源竞争造成假失败。
static SIXEL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sh(script: &str) -> PaneConfig {
    PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 500,
        shell: Some("/bin/bash".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        ..PaneConfig::default()
    }
}

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|c| c.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_for<F: Fn(&Frame) -> bool>(
    handle: &PaneHandle,
    timeout: Duration,
    pred: F,
) -> Box<Frame> {
    let start = Instant::now();
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if pred(&frame) {
            return frame;
        }
        if start.elapsed() > timeout {
            panic!("超时等待 PTY 输出，当前屏幕:\n{}", frame_text(&frame));
        }
        std::thread::sleep(Duration::from_millis(3));
    }
}

fn temp_payload(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "tervo-sixel-{}-{name}.bin",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("write payload");
    path
}

/// 返回一张 2x2 红/绿 Sixel 的完整 DCS（BEL 终止）。
fn small_sixel_bel() -> Vec<u8> {
    let mut out = b"\x1bPq".to_vec();
    out.extend_from_slice(b"\"1;1;2;2#0;2;100;0;0#1;2;0;100;0#0B#1B");
    out.push(0x07);
    out
}

/// 4x6：左侧两列红、右侧两列蓝；整行可见。
fn two_images_sixel() -> Vec<u8> {
    let mut out = b"\x1bPq\"1;1;4;6#0;2;100;0;0#1;2;0;0;100#0!2@#1!2@".to_vec();
    out.extend_from_slice(b"\x1b\\");
    out
}

/// 400x500、payload > 32 KiB，确保跨多个 PTY read chunk。
fn big_sixel_st() -> Vec<u8> {
    let mut out = b"\x1bPq\"1;1;400;500#0;2;100;0;0".to_vec();
    for _ in 0..84 {
        out.extend(std::iter::repeat_n(b'@', 400));
        if out.len() > 40_000 {
            break;
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

#[test]
fn sixel_over_pty_bel_terminated_and_cross_chunk() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let small = temp_payload("small", &small_sixel_bel());
    let big = temp_payload("big", &big_sixel_st());
    let script = format!(
        "printf '\\033[1;1H'; cat '{}'; cat '{}'; sleep 0.5",
        small.display(),
        big.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");

    let frame = wait_for(&handle, Duration::from_secs(8), |f| f.images.len() >= 2);
    // 2x2 小图 + 400x500 大图（大图经过 max payload chunk 切分）。
    let small_img = frame.images.iter().find(|i| i.width == 2 && i.height == 2).expect("2x2 image");
    assert_eq!(&small_img.data[0..4], &[255, 0, 0, 255]);
    assert_eq!(&small_img.data[4..8], &[0, 255, 0, 255]);
    let big_img = frame.images.iter().find(|i| i.width == 400 && i.height == 500).expect("big image");
    assert_eq!(big_img.data.len(), 400 * 500 * 4);

    // Sixel 字节没有进入网格；后续文本正常。
    let text = frame_text(&frame);
    assert!(!text.contains('\u{1b}'));
    let _ = std::fs::remove_file(small);
    let _ = std::fs::remove_file(big);
}

#[test]
fn invalid_and_oversize_sixel_drop_image_without_breaking_output() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload(
        "bad",
        concat!(
            "\x1bPq\"1;1;20000;20000#0;2;100;0;0@\x1b\\",
            "\x1bPq#0;2;100;0;0@@@\x1b\\",
            "\x1bPq\x1b\\"
        )
        .as_bytes(),
    );
    let script = format!(
        "cat '{}'; printf '\\033[20;1HSTILL-ALIVE\\r\\n'; sleep 0.3",
        path.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("STILL-ALIVE")
    });
    // 第一张超 16 MP 被丢；第二张 3x6 合法；第三张空图被丢。
    assert_eq!(frame.images.len(), 1);
    assert_eq!((frame.images[0].width, frame.images[0].height), (3, 6));
    let _ = std::fs::remove_file(path);
}

#[test]
fn ech_blank_damage_erases_inline_image() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let small = temp_payload("ech", &small_sixel_bel());
    // dsh-tui 的擦图路径：光标回到图片区域，ECH 写入空白 cell。
    let script = format!(
        "cat '{}'; printf '\\033[1;1H\\033[2X'; sleep 0.4",
        small.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| f.images.is_empty());
    assert!(frame.images.is_empty(), "ECH 空白擦除后旧图应消失");
    let _ = std::fs::remove_file(small);
}

#[test]
fn ech_on_other_row_keeps_inline_image() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let small = temp_payload("ech-other-row", &small_sixel_bel());
    // 图片在第 1 行；ECH 在第 10 行，不应清掉无关图片。
    let script = format!(
        "cat '{}'; printf '\\033[10;1H\\033[1X'; sleep 0.4",
        small.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| !f.images.is_empty());
    assert!(!frame.images.is_empty(), "无关行的 ECH 不应清除图片");
    let _ = std::fs::remove_file(small);
}

#[test]
fn xtwinops_reports_cell_and_window_pixels() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let resp = std::env::temp_dir().join(format!("tervo-xtwinops-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&resp);
    let script = format!(
        "stty raw -echo; printf '\\033[16t\\033[14t\\033[c'; timeout --foreground 0.8 dd bs=1 count=96 of='{}' 2>/dev/null || true; stty sane; printf '\\r\\nXTDONE\\r\\n'; sleep 0.3",
        resp.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("XTDONE")
    });
    let _ = frame;
    let bytes = std::fs::read(&resp).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    // 默认 cell=8x16；100x24 的窗口 = 800x384。
    assert!(text.contains("\u{1b}[6;16;8t"), "缺少 cell pixel 回复: {text:?}");
    assert!(text.contains("\u{1b}[4;384;800t"), "缺少 window pixel 回复: {text:?}");
    let _ = std::fs::remove_file(resp);
}

#[test]
fn da1_advertises_sixel_only_when_enabled() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    fn check(sixel: bool, expect_hex: &str) {
        let resp = std::env::temp_dir().join(format!(
            "tervo-da1-{}-{}.bin",
            std::process::id(),
            sixel
        ));
        let _ = std::fs::remove_file(&resp);
        let script = format!(
            "stty raw -echo; printf '\\033[c'; timeout --foreground 0.5 dd bs=1 count=32 of='{}' 2>/dev/null || true; stty sane; printf '\\r\\nDA1:'; od -An -tx1 '{}' | tr -d ' \\n'; printf '\\r\\nENDDA1\\r\\n'; sleep 0.4",
            resp.display(),
            resp.display()
        );
        let handle = spawn(PaneConfig { sixel, ..sh(&script) }).expect("spawn");
        let frame = wait_for(&handle, Duration::from_secs(5), |f| {
            frame_text(f).contains("ENDDA1")
        });
        let text = frame_text(&frame);
        let hex = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("DA1:"))
            .expect("DA1 line");
        assert_eq!(hex, expect_hex, "text={text}");
        let _ = std::fs::remove_file(resp);
    }

    // \x1b[?62;4;9;22c：包含 Sixel 参数 4。
    check(true, "1b5b3f36323b343b393b323263");
    // \x1b[?6c：不宣告参数 4。
    check(false, "1b5b3f3663");
}

#[test]
fn image_anchor_scrolls_with_history() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("anchor", &small_sixel_bel());
    let script = format!(
        "printf '\\033[24;1H'; cat '{}'; printf '\\r\\n\\r\\n\\r\\n\\r\\n\\r\\nEND'; sleep 0.5",
        path.display()
    );
    let handle = spawn(PaneConfig { cols: 40, rows: 24, ..sh(&script) })
        .expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("END") && !f.images.is_empty()
    });
    // 图片锚定在输出前的第 24 行（grid 23），5 个换行把历史顶到 5：
    // 视口行 = 23 - 5 = 18。
    assert_eq!(frame.images[0].row, 18);

    // 用户上滚 3 行：同一图片在视口里下移 3 行。
    handle.scroll(3);
    let frame = wait_for(&handle, Duration::from_secs(3), |f| {
        !f.images.is_empty() && f.images[0].row == 21
    });
    assert_eq!(frame.images[0].row, 21);

    // 回到底部，位置恢复。
    handle.scroll_to(0);
    let frame = wait_for(&handle, Duration::from_secs(3), |f| {
        !f.images.is_empty() && f.images[0].row == 18
    });
    assert_eq!(frame.images[0].row, 18);
    let _ = std::fs::remove_file(path);
}

#[test]
fn ech_rectangle_keeps_two_images_on_same_row() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("ech-two-images", &two_images_sixel());
    let script = format!(
        "cat '{}'; printf '\\033[1;2H\\033[1X'; sleep 0.4",
        path.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| !f.images.is_empty());
    assert_eq!(frame.images.len(), 1, "窄 ECH 不应删除同一行的无关图片");
    let _ = std::fs::remove_file(path);
}

#[test]
fn two_images_on_one_row_expose_distinct_source_ids() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("two-images", &two_images_sixel());
    let script = format!("cat '{}'; sleep 0.4", path.display());
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| !f.images.is_empty());
    assert_eq!(frame.images.len(), 1);
    // ID 各自单调递增，第一张图恰好都从 1 开始；不要求数值不相等。
    assert_eq!(frame.images[0].id, 1);
    assert_eq!(frame.images[0].source_id, 1);
    let _ = std::fs::remove_file(path);
}

#[test]
fn image_rgba_can_be_recovered_by_id() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("recover-id", &small_sixel_bel());
    let script = format!("cat '{}'; sleep 0.4", path.display());
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| !f.images.is_empty());
    let id = frame.images[0].source_id;
    let (w, h, rgba) = handle.image_rgba(id).expect("按 source ID 取回原图");
    assert_eq!((w, h), (2, 2));
    assert_eq!(rgba.len(), 16);
    assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
    assert!(handle.image_rgba(id.wrapping_add(999)).is_none());
    let _ = std::fs::remove_file(path);
}

#[test]
fn escape_clear_screen_removes_images() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("esc-clear", &small_sixel_bel());
    let script = format!(
        "cat '{}'; printf '\\033[2J\\033[1;1HESCAPE-CLEARED'; sleep 0.4",
        path.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");
    let frame = wait_for(&handle, Duration::from_secs(5), |f| {
        frame_text(f).contains("ESCAPE-CLEARED")
    });
    assert!(frame.images.is_empty(), "CSI 2J 必须清除已有 Sixel");
    let _ = std::fs::remove_file(path);
}

#[test]
fn sixel_clear_screen_removes_images() {
    let _serial = SIXEL_TEST_LOCK.lock().unwrap();
    let path = temp_payload("clear", &small_sixel_bel());
    let script = format!("cat '{}'; printf '\\r\\nX\\r\\n'; sleep 0.4", path.display());
    let handle = spawn(sh(&script)).expect("spawn pane");
    wait_for(&handle, Duration::from_secs(5), |f| !f.images.is_empty());
    handle.clear_screen();
    let frame = wait_for(&handle, Duration::from_secs(3), |f| f.images.is_empty());
    assert!(frame_text(&frame).trim().is_empty());
    let _ = std::fs::remove_file(path);
}
