// SPDX-License-Identifier: GPL-3.0-only
//! P0 UI 阻塞回归：慢 shell + 大粘贴 + 输出洪峰下，UI 路径（input/pull）必须有界。
//!
//! 这些测试刻意不复用 GUI：它们测的是 app 调用的同一层 API（`PaneHandle::input`
//! 与 `PaneHandle::pull`）。PTY 用真实 shell，慢速/不读 stdin 的行为由脚本制造。

use std::time::{Duration, Instant};

use tervo_core::{spawn, Frame, PaneConfig, PaneHandle};

const INPUT_BYTES: usize = 10 * 1024 * 1024;

fn sh(script: &str) -> PaneConfig {
    PaneConfig {
        cols: 200,
        rows: 50,
        scrollback: 2000,
        shell: Some("/bin/sh".to_string()),
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

/// 等屏幕出现目标文本；返回 (frame, 最大单次 pull 耗时)。
fn wait_text_bounded(
    handle: &PaneHandle,
    needle: &str,
    timeout: Duration,
) -> (Box<Frame>, Duration) {
    let start = Instant::now();
    let mut frame = handle.new_frame();
    let mut worst = Duration::ZERO;
    loop {
        let t = Instant::now();
        frame = handle.pull(frame);
        worst = worst.max(t.elapsed());
        if !frame.skipped && frame_text(&frame).contains(needle) {
            return (frame, worst);
        }
        assert!(
            start.elapsed() < timeout,
            "等待 {needle:?} 超时；屏幕片段:\n{}",
            frame_text(&frame)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn patterned_payload() -> Vec<u8> {
    // 字母循环模式，任何丢字节/乱序都会让整文件比较失败。
    (0..INPUT_BYTES).map(|i| b'A' + (i % 26) as u8).collect()
}

/// 慢 shell（先输出 8 MiB 洪峰、期间完全不读 stdin）下粘贴 10 MiB：
/// - `input()` 每次调用必须快速返回（UI 线程同一路径）；
/// - `pull()` 单次有界（超时跳帧，而不是等 actor 写完 PTY）；
/// - shell 开始读 stdin 后必须收到完整、顺序一致的 10 MiB。
#[test]
fn slow_shell_10mb_paste_and_flood_never_block_ui() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("tervo-p0-paste-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let script = format!(
        "stty raw -echo 2>/dev/null || true\n\
         printf 'READY\\r\\n'\n\
         yes TERVO-FLOOD | head -c 8388608 >/dev/null\n\
         printf 'BEGIN\\r\\n'\n\
         head -c {INPUT_BYTES} > '{}'\n\
         printf 'GOT %s\\r\\n' \"$(wc -c < '{}')\"\n\
         sleep 2",
        path.display(),
        path.display()
    );
    let handle = spawn(sh(&script)).expect("spawn pane");

    // 先等脚本进入 raw 模式并开始洪峰输出（READY 在洪峰之前打印）。
    let (_frame, _) = wait_text_bounded(&handle, "READY", Duration::from_secs(10));

    let payload = patterned_payload();
    let mut worst_input = Duration::ZERO;
    // 拆成多块，验证 backlog 内部的 FIFO 顺序。
    for chunk in payload.chunks(2_500_000) {
        let t = Instant::now();
        handle.input(chunk);
        worst_input = worst_input.max(t.elapsed());
    }

    let (frame, worst_pull) =
        wait_text_bounded(&handle, &format!("GOT {INPUT_BYTES}"), Duration::from_secs(40));
    assert!(
        frame_text(&frame).contains(&format!("GOT {INPUT_BYTES}")),
        "shell 没读满 10 MiB: {}",
        frame_text(&frame)
    );

    // shell 已退出 head，文件应逐字节等于粘贴内容（无丢失、无乱序、无 PTY 转换）。
    let written = std::fs::read(&path).expect("read pasted file");
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        written.len(),
        INPUT_BYTES,
        "写入字节数不一致（丢弃/截断）"
    );
    assert_eq!(written, payload, "10 MiB 粘贴内容被改写或乱序");

    eprintln!("[p0] 10MiB paste worst_input={worst_input:?} worst_pull={worst_pull:?}");
    let limit = Duration::from_millis(120);
    assert!(
        worst_input < limit,
        "input() 在慢 shell 下阻塞 UI: {worst_input:?} >= {limit:?}"
    );
    // pull 硬超时 100ms；留一点调度余量。
    let pull_limit = Duration::from_millis(250);
    assert!(
        worst_pull < pull_limit,
        "pull() 在洪峰/慢 shell 下卡住: {worst_pull:?} >= {pull_limit:?}"
    );
}

/// actor 已退出/不再回帧时，`pull()` 必须按硬超时返回 `skipped`，不能无限等。
#[test]
fn pull_after_actor_shutdown_is_bounded_and_skipped() {
    let mut handle = spawn(sh("sleep 10")).expect("spawn pane");
    handle.kill();
    std::thread::sleep(Duration::from_millis(150));

    let t = Instant::now();
    let frame = handle.pull(handle.new_frame());
    let cost = t.elapsed();
    assert!(frame.skipped, "actor 退出后 pull 应返回 skipped 帧");
    assert!(
        cost < Duration::from_millis(300),
        "actor 退出后 pull 卡住: {cost:?}"
    );
    assert!(
        handle
            .stats
            .pull_timeouts
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}

#[test]
fn backlog_overflow_drops_with_counters_and_keeps_calls_bounded() {
    let handle = spawn(sh("sleep 30")).expect("spawn pane");
    // 让 shell 先起来；sleep 不会读 stdin。
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(handle.stats.write_dropped_bytes.load(std::sync::atomic::Ordering::Relaxed), 0);

    let chunk = vec![b'x'; 1024 * 1024];
    let mut worst_input = Duration::ZERO;
    // 20 MiB > 16 MiB backlog；writer 阻塞在内核写，最终会触发明确丢弃。
    for _ in 0..20 {
        let t = Instant::now();
        handle.input(&chunk);
        worst_input = worst_input.max(t.elapsed());
    }

    // actor 消费输入队列需要一点时间；等待丢弃计数出现。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let dropped = handle
            .stats
            .write_dropped_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        if dropped > 0 {
            eprintln!(
                "[p0] 慢 shell backlog 溢出: dropped_bytes={dropped} chunks={}",
                handle
                    .stats
                    .write_dropped_chunks
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "20 MiB 输入没有触发有界丢弃；write backlog 可能无界"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // 即使 backlog 溢出，拉帧也不能被写线程拖死。
    let t = Instant::now();
    let frame = handle.pull(handle.new_frame());
    let pull_cost = t.elapsed();
    assert!(frame.rows > 0);
    assert!(
        pull_cost < Duration::from_millis(250),
        "backlog 溢出时 pull 卡住: {pull_cost:?}"
    );
    assert!(
        worst_input < Duration::from_millis(120),
        "backlog 溢出时 input 阻塞 UI: {worst_input:?}"
    );
}

/// 写 backlog 饱和（慢 reader + 20 MiB 输入）时，copy/search/contents 等同步
/// ctrl 回复仍有硬上限，不能把 UI 冻在 500ms。
#[test]
fn ctrl_replies_stay_bounded_under_write_backpressure() {
    let handle = spawn(sh(
        "stty raw -echo; while true; do dd bs=1 count=1 of=/dev/null 2>/dev/null; sleep 0.01; done",
    ))
    .expect("spawn pane");
    std::thread::sleep(Duration::from_millis(250));

    let chunk = vec![b'x'; 1024 * 1024];
    for _ in 0..20 {
        handle.input(&chunk);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle
        .stats
        .write_dropped_bytes
        .load(std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        assert!(Instant::now() < deadline, "20 MiB 未触发写 backlog 背压");
        std::thread::sleep(Duration::from_millis(10));
    }

    let t = Instant::now();
    handle.select_all();
    let copy = handle.copy_selection();
    let copy_cost = t.elapsed();
    let _ = copy; // 空白屏可能为 None；这里只断言耗时有界。

    let t = Instant::now();
    let search = handle.search_with("x", true, tervo_core::SearchOptions::default());
    let search_cost = t.elapsed();
    let _ = search;

    let t = Instant::now();
    let contents = handle.contents();
    let contents_cost = t.elapsed();
    let _ = contents;

    eprintln!(
        "[p0] ctrl under backpressure: copy={copy_cost:?} search={search_cost:?} contents={contents_cost:?}"
    );
    let limit = Duration::from_millis(500);
    assert!(copy_cost < limit, "copy_selection 卡住: {copy_cost:?}");
    assert!(search_cost < limit, "search_with 卡住: {search_cost:?}");
    assert!(contents_cost < limit, "contents 卡住: {contents_cost:?}");
}
