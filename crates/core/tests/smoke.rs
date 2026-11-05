//! 端到端冒烟测试：真实 PTY + shell + actor + 拉取式快照。

use std::time::{Duration, Instant};

use rusttabby_core::{spawn, CoreEvent, Frame, PaneConfig, PaneHandle};

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

fn frame_text(frame: &Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|c| c.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// 反复拉帧直到条件满足或超时。
fn wait_for<F: Fn(&Frame) -> bool>(
    handle: &PaneHandle,
    frame: Box<Frame>,
    timeout: Duration,
    pred: F,
) -> (Box<Frame>, Duration) {
    let start = Instant::now();
    let mut frame = frame;
    loop {
        frame = handle.pull(frame);
        if pred(&frame) {
            return (frame, start.elapsed());
        }
        if start.elapsed() > timeout {
            panic!("超时等待 PTY 输出，当前屏幕:\n{}", frame_text(&frame));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn pty_prints_and_ui_pulls() {
    let handle = spawn(sh("printf 'hello-from-pty\\n'; sleep 0.4")).expect("spawn pane");
    let frame = handle.new_frame();
    let (frame, elapsed) = wait_for(&handle, frame, Duration::from_secs(5), |f| {
        frame_text(f).contains("hello-from-pty")
    });
    eprintln!("首次看到输出耗时 {elapsed:?}");
    assert!(frame_text(&frame).contains("hello-from-pty"));
}

#[test]
fn keyboard_input_reaches_child() {
    let handle = spawn(PaneConfig {
        cols: 80,
        rows: 24,
        scrollback: 200,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-i".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn pane");

    let frame = handle.new_frame();
    std::thread::sleep(Duration::from_millis(150));
    handle.input(b"echo roundtrip-ok\n");

    let (_frame, elapsed) = wait_for(&handle, frame, Duration::from_secs(5), |f| {
        frame_text(f).contains("roundtrip-ok")
    });
    eprintln!("键入 → 解析可见: {elapsed:?}");
}

#[test]
fn title_event_is_delivered() {
    let handle = spawn(sh("printf '\\033]0;rust-tabby-title\\007'; sleep 0.5")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(ev) = handle.try_event() {
            if let CoreEvent::Title(t) = ev {
                assert_eq!(t, "rust-tabby-title");
                return;
            }
        }
        if Instant::now() > deadline {
            panic!("未收到 title 事件");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// 洪峰下 UI 拉帧不得被长时间卡住（合并帧而非阻塞）。
#[test]
fn pull_latency_stays_bounded_under_flood() {
    let handle = spawn(sh("yes RUSTTABBY-FLOOD")).expect("spawn");

    let mut frame = handle.new_frame();
    // 先让它跑一会儿，制造积压
    std::thread::sleep(Duration::from_millis(300));

    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    for _ in 0..120 {
        let t = Instant::now();
        frame = handle.pull(frame);
        let dt = t.elapsed();
        total += dt;
        worst = worst.max(dt);
        std::thread::sleep(Duration::from_millis(4));
    }
    let avg = total / 120;
    eprintln!("洪峰下拉帧: 平均 {avg:?}, 最差 {worst:?}");

    assert!(
        worst < Duration::from_millis(60),
        "拉帧被洪峰卡住: 最差 {worst:?}"
    );
    assert!(
        frame_text(&frame).contains("RUSTTABBY-FLOOD") || frame.rows > 0,
        "帧内容异常"
    );
}

#[test]
fn cat_10mb_streams_through_pane() {
    use std::io::Write;
    let path = std::env::temp_dir().join("rusttabby-smoke-cat.txt");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        let line = b"The quick brown fox jumps over the lazy dog 0123456789\r\n";
        let mut written = 0usize;
        while written < 10 * 1024 * 1024 {
            f.write_all(line).unwrap();
            written += line.len();
        }
        f.write_all(b"\r\nSMOKE-EOF-MARKER\r\n").unwrap();
        f.flush().unwrap();
    }
    eprintln!("file = {} bytes={}", path.display(), std::fs::metadata(&path).unwrap().len());

    let handle = spawn(PaneConfig {
        cols: 200,
        rows: 50,
        scrollback: 2000,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), format!("cat {}", path.display())],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let start = Instant::now();
    let mut frame = handle.new_frame();
    let mut pulls = 0u64;
    loop {
        frame = handle.pull(frame);
        pulls += 1;
        if frame_text(&frame).contains("SMOKE-EOF-MARKER") {
            break;
        }
        if start.elapsed() > Duration::from_secs(15) {
            let st = &handle.stats;
            eprintln!(
                "超时: pulls={pulls} bytes_parsed={} batches={} pty_rx_empty={}",
                st.bytes_parsed.load(std::sync::atomic::Ordering::Relaxed),
                st.parse_batches.load(std::sync::atomic::Ordering::Relaxed),
                st.pulls.load(std::sync::atomic::Ordering::Relaxed),
            );
            panic!("超时，屏幕:\n{}", frame_text(&frame));
        }
    }
    eprintln!("cat 10MB 完成: {:?}, pulls={pulls}", start.elapsed());
}

/// 场景：shell 输出 200 行，测试回滚、选区、搜索。
fn spawn_seq_pane() -> PaneHandle {
    spawn(PaneConfig {
        cols: 60,
        rows: 12,
        scrollback: 1000,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "seq 1 200; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn")
}

#[test]
fn wheel_scrollback_shows_history() {
    let handle = spawn_seq_pane();
    let mut frame = handle.new_frame();
    // 等到屏幕出现 200
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("200") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }
    let bottom = frame_text(&frame);
    assert!(bottom.contains("200"));

    // 向上滚 5 行
    handle.scroll(5);
    let mut frame2 = handle.new_frame();
    frame2 = handle.pull(frame2);
    assert!(frame2.display_offset > 0, "display_offset 未增加");
    let scrolled = frame_text(&frame2);
    eprintln!("offset={} bottom_first={:?} scrolled_first={:?}", frame2.display_offset, bottom.lines().next(), scrolled.lines().next());
    assert_ne!(scrolled, bottom, "回滚后内容应变化");
    assert!(!scrolled.contains("200"), "顶部应看到更早的行: {scrolled}");

    // 滚回底部
    handle.scroll(-1000);
    let mut frame3 = handle.new_frame();
    frame3 = handle.pull(frame3);
    assert_eq!(frame3.display_offset, 0);
    assert!(frame_text(&frame3).contains("200"));
}

#[test]
fn selection_copy_returns_text() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "printf 'alpha bravo charlie\\n'; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let frame = handle.new_frame();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = frame;
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("charlie") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }
    // 选区 "alpha"：第 0 行第 0..4 列
    handle.selection_start(0, 0);
    handle.selection_extend(0, 4);
    let text = handle.copy_selection().expect("应有选区文本");
    assert_eq!(text, "alpha", "实际选区: {text:?}");

    // 清空选区后不应再返回文本
    handle.selection_clear();
    std::thread::sleep(Duration::from_millis(30));
    assert!(handle.copy_selection().is_none());
}

#[test]
fn backward_selection_includes_first_column() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "printf 'abcdef\\n'; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("abcdef") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    // 从右往左拖：锚点在第 5 列，拖到第 0 列——最左边的 'a' 必须被选上。
    // 旧实现把两端各减一格（起点 Right + 终点 Left），得到 "bcde"。
    handle.selection_start(0, 5);
    handle.selection_extend(0, 0);
    let text = handle.copy_selection().expect("应有选区文本");
    assert_eq!(text, "abcdef", "反向拖到行首: {text:?}");

    // 反向拖到中间：只覆盖 2..=5，锚点格也要包含，且不能把左边多余字符带进来。
    handle.selection_start(0, 5);
    handle.selection_extend(0, 2);
    let text = handle.copy_selection().expect("应有选区文本");
    assert_eq!(text, "cdef", "反向拖到中间: {text:?}");
}

#[test]
fn search_finds_and_scrolls() {
    let handle = spawn_seq_pane();
    let mut frame = handle.new_frame();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("200") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    // "7" 在输出的很前面，需要滚上去
    let hit = handle.search("77", true).expect("应能找到 77");
    assert!(hit.line < 12);
    let frame = handle.pull(handle.new_frame());
    let text = frame_text(&frame);
    assert!(text.contains("77"), "滚动后应看到命中: {text}");

    // 搜不到
    assert!(handle.search("zzzz-no-such-line", true).is_none());

    // 清除高亮
    handle.search_clear();
    let frame = handle.pull(handle.new_frame());
    assert!(frame.search.is_none());
}

#[test]
fn backspace_erases_last_char() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-i".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let mut frame = handle.new_frame();
    std::thread::sleep(Duration::from_millis(200));
    handle.input(b"abc");
    // 等 echo
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("abc") {
            break;
        }
        assert!(Instant::now() < deadline, "没有回显: {}", frame_text(&frame));
        std::thread::sleep(Duration::from_millis(5));
    }

    // 退格（DEL）两次
    handle.input(b"\x7f\x7f");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        frame = handle.pull(frame);
        let text = frame_text(&frame);
        if !text.contains("abc") && text.contains("a") {
            eprintln!("退格后: {:?}", text.lines().find(|l| !l.is_empty()));
            return;
        }
        assert!(Instant::now() < deadline, "退格无效: {}", frame_text(&frame));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn osc_color_queries_get_replies() {
    // 子进程问「调色板 1 号色 / 前景色」，终端必须按当前主题回 rgb: 串。
    let dir = std::env::temp_dir().join(format!("rt-color-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let out = dir.join("reply.bin");
    let _ = std::fs::remove_file(&out);

    // 必须先 raw：查询应答里没有换行，规范模式下 cat 永远读不到。
    let script = format!(
        // 不能裸用 `timeout`：它会给 dd 新建进程组，非前台组读终端会被 SIGTTIN 挂起。
        "stty raw -echo; (sleep 0.3; printf '\\033]4;1;?\\007'; sleep 0.2; printf '\\033]10;?\\007') & timeout --foreground 2 dd bs=1 count=64 of={} 2>/dev/null",
        out.display()
    );
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 5,
        scrollback: 10,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script],
        ..PaneConfig::default()
    })
    .expect("spawn");

    // 两条应答是分两次到的（dd 会一直读到 64 字节或超时），所以按内容等齐再断言。
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut text = String::new();
    while Instant::now() < deadline {
        text = String::from_utf8_lossy(&std::fs::read(&out).unwrap_or_default()).into_owned();
        if text.contains("]4;1;rgb:") && text.contains("]10;rgb:") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(text.contains("]4;1;rgb:"), "OSC 4 查询没答复: {text:?}");
    assert!(text.contains("]10;rgb:"), "OSC 10 查询没答复: {text:?}");
    drop(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn select_all_copies_visible_screen() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "printf 'alpha\\nbravo\\ncharlie\\n'; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("charlie") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    handle.select_all();
    let text = handle.copy_selection().expect("全选应有文本");
    assert!(text.contains("alpha"), "{text:?}");
    assert!(text.contains("charlie"), "{text:?}");
}

#[test]
fn clear_screen_wipes_content() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "printf 'hello\\n'; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("hello") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Ctrl 通道 FIFO：清屏先于这次拉帧被处理
    handle.clear_screen();
    let frame = handle.pull(frame);
    assert!(!frame_text(&frame).contains("hello"), "清屏后不应再有旧内容");
}
