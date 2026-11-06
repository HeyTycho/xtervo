// SPDX-License-Identifier: GPL-3.0-only
//! 端到端冒烟测试：真实 PTY + shell + actor + 拉取式快照。

use std::time::{Duration, Instant};

use tervo_core::{spawn, CoreEvent, Frame, PaneConfig, PaneHandle, SearchOptions};

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

#[test]
fn osc_progress_notification_and_bell_are_delivered() {
    // 一次 shell 脚本里依次发进度、OSC 9、OSC 777、BEL，验证 actor 旁路扫描真的接上了。
    let handle = spawn(sh(concat!(
        "printf '\\033]9;4;1;42\\007';",
        "printf '\\033]9;hello\\007';",
        "printf '\\033]777;notify;Build;done\\007';",
        "printf '\\007'; sleep 0.5"
    )))
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut progress = None;
    let mut notes = Vec::new();
    let mut bell = false;
    while Instant::now() < deadline {
        while let Some(event) = handle.try_event() {
            match event {
                CoreEvent::Progress(p) => progress = Some(p),
                CoreEvent::Notification(n) => notes.push(n),
                CoreEvent::Bell => bell = true,
                _ => {},
            }
        }
        if progress.is_some() && notes.len() >= 2 && bell {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(
        progress,
        Some(tervo_core::Progress {
            state: tervo_core::ProgressState::Normal,
            percent: Some(42),
        }),
        "OSC 9;4 进度事件"
    );
    assert_eq!(notes.len(), 2, "OSC 9 + OSC 777 通知: {notes:?}");
    assert_eq!(notes[0].title, "");
    assert_eq!(notes[0].body, "hello");
    assert_eq!(notes[1].title, "Build");
    assert_eq!(notes[1].body, "done");
    assert!(bell, "BEL 事件");
}

/// 洪峰下 UI 拉帧不得被长时间卡住（合并帧而非阻塞）。
#[test]
fn pull_latency_stays_bounded_under_flood() {
    let handle = spawn(sh("yes TERVO-FLOOD")).expect("spawn");

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
        frame_text(&frame).contains("TERVO-FLOOD") || frame.rows > 0,
        "帧内容异常"
    );
}

#[test]
fn cat_10mb_streams_through_pane() {
    use std::io::Write;
    let path = std::env::temp_dir().join("tervo-smoke-cat.txt");
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

    // 非阻塞滚动位置查询：底部 offset=0，history 已有内容，rows 与视口一致。
    let pos = handle.scroll_position();
    assert_eq!(pos.offset, 0, "新输出后应在底部");
    assert_eq!(pos.rows, 12);
    assert!(pos.history > 0, "seq 200 后应有 scrollback: {pos:?}");
    assert!(pos.scrollable());
    assert_eq!(pos.total(), pos.history + pos.rows);
    assert!(pos.thumb(0.08).is_some());

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

    // 绝对位置滚动（滚动条拖拽路径）：滚到顶/中间/底，位置查询必须同步。
    let history = handle.scroll_position().history;
    handle.scroll_to(history);
    let _ = handle.pull(handle.new_frame());
    assert_eq!(handle.scroll_position().offset, history, "scroll_to 顶部");
    handle.scroll_to(3);
    let _ = handle.pull(handle.new_frame());
    assert_eq!(handle.scroll_position().offset, 3, "scroll_to 绝对位置");
    handle.scroll_to(usize::MAX);
    let _ = handle.pull(handle.new_frame());
    assert_eq!(handle.scroll_position().offset, history, "越界夹到顶部");
    handle.scroll_to(0);
    let _ = handle.pull(handle.new_frame());
    assert_eq!(handle.scroll_position().offset, 0);
}

/// 等 actor 的非阻塞滚动快照满足条件；不依赖拉帧（拉帧只影响渲染）。
fn wait_scroll<F: Fn() -> bool>(handle: &PaneHandle, pred: F, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !pred() {
        assert!(Instant::now() < deadline, "等待超时: {what}（当前 {:?}）", handle.scroll_position());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 两批输出之间用 `read` 暂停，测试可确定性地在上滚后触发第二批输出。
fn spawn_scroll_policy_pane(scroll_on_output: bool) -> PaneHandle {
    spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 500,
        shell: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            "seq 1 20; printf 'READY\\n'; read x; seq 21 60; sleep 3".to_string(),
        ],
        scroll_on_output,
        ..PaneConfig::default()
    })
    .expect("spawn scroll policy pane")
}

/// 等到 READY 出现后上滚，返回上滚前的 history。
fn scroll_up_after_ready(handle: &PaneHandle) -> usize {
    let mut frame = handle.new_frame();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("READY") {
            break;
        }
        assert!(Instant::now() < deadline, "没等到 READY 输出");
        std::thread::sleep(Duration::from_millis(5));
    }
    let base = handle.scroll_position().history;
    assert!(base > 0, "READY 后应有历史: {base}");
    handle.scroll(5);
    wait_scroll(handle, || handle.scroll_position().offset > 0, "滚轮上滚");
    handle.scroll_position().history
}

/// 等第二批输出基本结束：history 达到阈值后连续 3 次采样不再增长。
fn wait_output_settled(handle: &PaneHandle, min_history: usize) {
    wait_scroll(handle, || handle.scroll_position().history >= min_history, "第二批输出");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last = handle.scroll_position().history;
    let mut stable = 0;
    while stable < 3 {
        assert!(Instant::now() < deadline, "输出迟迟不稳定（history={last}）");
        std::thread::sleep(Duration::from_millis(20));
        let now = handle.scroll_position().history;
        if now == last {
            stable += 1;
        } else {
            last = now;
            stable = 0;
        }
    }
}

#[test]
fn scroll_on_output_false_keeps_user_history_view() {
    let handle = spawn_scroll_policy_pane(false);
    let base = scroll_up_after_ready(&handle);
    let before = handle.scroll_position();
    assert!(before.offset > 0);

    // 放行第二批输出；用户仍停在原历史位置，新输出不能抢回底部。
    handle.input(b"\n");
    wait_output_settled(&handle, base + 35);

    let frame = handle.pull(handle.new_frame());
    let pos = handle.scroll_position();
    assert!(pos.offset > 0, "scroll_on_output=false 时视图被新输出抢回底部: {pos:?}");
    assert!(frame.display_offset > 0, "帧 offset 也应保持: {}", frame.display_offset);
    assert!(
        !frame_text(&frame).contains("60"),
        "上滚后不应直接看到最新输出：\n{}",
        frame_text(&frame)
    );
}

#[test]
fn scroll_on_output_true_follows_new_output() {
    let handle = spawn_scroll_policy_pane(true);
    let base = scroll_up_after_ready(&handle);
    assert!(handle.scroll_position().offset > 0);

    handle.input(b"\n");
    wait_output_settled(&handle, base + 35);

    let frame = handle.pull(handle.new_frame());
    assert_eq!(frame.display_offset, 0, "scroll_on_output=true 应回到底部");
    assert_eq!(handle.scroll_position().offset, 0);
    assert!(frame_text(&frame).contains("60"), "应看到最新输出：\n{}", frame_text(&frame));
}

#[test]
fn scroll_to_bottom_resets_offset_after_keyboard_navigation() {
    let handle = spawn_scroll_policy_pane(false);
    scroll_up_after_ready(&handle);
    assert!(handle.scroll_position().offset > 0);

    handle.scroll_to_bottom();
    wait_scroll(&handle, || handle.scroll_position().offset == 0, "scroll_to_bottom");
    let frame = handle.pull(handle.new_frame());
    assert_eq!(frame.display_offset, 0);
}

#[test]
fn set_scroll_on_output_takes_effect_on_existing_pane() {
    // spawn 时关闭；用户上滚后再从设置页打开，第二批输出应把视图拉回底部。
    let handle = spawn_scroll_policy_pane(false);
    let base = scroll_up_after_ready(&handle);
    assert!(handle.scroll_position().offset > 0);

    handle.set_scroll_on_output(true);
    // Pull 排在 SetScrollOnOutput 之后，返回时 actor 已处理开关（异步无阻塞）。
    let _ = handle.pull(handle.new_frame());
    handle.input(b"\n");
    wait_output_settled(&handle, base + 35);

    let frame = handle.pull(handle.new_frame());
    assert_eq!(frame.display_offset, 0, "运行时开启后输出应回到底部");
    assert!(frame_text(&frame).contains("60"), "应看到最新输出：\n{}", frame_text(&frame));
}

#[test]
fn scroll_position_tracks_resize_without_blocking() {
    let mut handle = spawn(PaneConfig {
        cols: 60,
        rows: 12,
        scrollback: 1000,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "seq 1 100; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    // 等输出 + 拉一帧，确保 actor 已发布快照。
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("100") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }
    let pos = handle.scroll_position();
    assert_eq!(pos.rows, 12);
    assert!(pos.history > 0);

    // resize 后 rows 立即跟上（UI 侧 grid 同步），actor 处理完 pull 后 history 一致。
    handle.resize(80, 6);
    let _ = handle.pull(handle.new_frame());
    let resized = handle.scroll_position();
    assert_eq!(resized.rows, 6);
    assert!(resized.history > 0, "resize 不应清掉 scrollback: {resized:?}");
    assert!(resized.scrollable());
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
    // 计数同样来自 alacritty grid：seq 1..200 里 "77" 出现在 77 与 177。
    assert_eq!((hit.index, hit.total), (1, 2), "命中计数应为 1/2: {hit:?}");
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

#[test]
fn select_line_selects_single_visible_line() {
    let handle = spawn(PaneConfig {
        cols: 30,
        rows: 6,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "printf 'aaa\\nbbb\\n'; sleep 3".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("bbb") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    handle.select_line(0);
    let text = handle.copy_selection().expect("选行应有文本");
    let line = text.trim_end();
    assert_eq!(line, "aaa", "只应选中第一行: {text:?}");
}

#[test]
fn search_supports_case_and_regex_options() {
    let handle = spawn(PaneConfig {
        cols: 60,
        rows: 8,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            "printf 'Alpha alpha\\nabc123\\n'; sleep 3".to_string(),
        ],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("abc123") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    let case = SearchOptions { match_case: true, regex: false };
    let nocase = SearchOptions { match_case: false, regex: false };
    let regex_case = SearchOptions { match_case: true, regex: true };

    // 显式 Match case：大写查询不能命中 "Alpha"/"alpha"。
    handle.search_clear();
    assert!(
        handle.search_with("ALPHA", true, case).expect("valid plain query").is_none(),
        "Match case 开启时 ALPHA 不应命中 Alpha/alpha"
    );

    // 关闭 Match case：大小写不敏感。
    handle.search_clear();
    let hit = handle
        .search_with("ALPHA", true, nocase)
        .expect("valid plain query")
        .expect("Match case 关闭时 ALPHA 应命中");
    assert_eq!(hit.total, 2, "Alpha/alpha 应计 2 个匹配: {hit:?}");
    assert!((1..=2).contains(&hit.index), "index 应在 1..=2: {hit:?}");

    // 纯文本模式下 "." 是字面量；regex 模式下才是通配符。
    handle.search_clear();
    assert!(
        handle.search_with("a.c", true, case).expect("valid plain query").is_none(),
        "纯文本模式不应把 . 当通配符"
    );
    handle.search_clear();
    let hit = handle
        .search_with("a.c", true, regex_case)
        .expect("valid regex")
        .expect("regex 应命中 abc");
    // 复用已有内容帧：命中在视口内不会触发滚屏，新帧只会拿到增量 damage。
    frame = handle.pull(frame);
    assert!(frame.search.is_some(), "命中应有高亮: {hit:?}");
    assert!(frame_text(&frame).contains("abc123"));

    // 非法 regex 必须返回错误（UI 显示），而不是伪装成 no match。
    handle.search_clear();
    let err = handle.search_with("[", true, regex_case).expect_err("非法 regex 应报错");
    assert!(err.contains("invalid regex"), "{err}");
    frame = handle.pull(frame);
    assert!(frame.search.is_none(), "非法 regex 不应保留旧高亮");
}

#[test]
fn search_reports_match_index_and_total_across_scrollback() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 6,
        scrollback: 200,
        shell: Some("/bin/sh".to_string()),
        args: vec![
            "-c".to_string(),
            "printf 'foo foo\\nbar foo\\n'; sleep 3".to_string(),
        ],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("bar foo") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    // 光标停在最后的空行；向前搜索会先绕回第一行的 "foo"。
    handle.search_clear();
    let opts = SearchOptions::default();
    let first = handle
        .search_with("foo", true, opts)
        .expect("valid plain query")
        .expect("应命中 foo");
    assert_eq!((first.index, first.total), (1, 3), "首个命中应为 1/3: {first:?}");
    let second = handle
        .search_with("foo", true, opts)
        .expect("valid plain query")
        .expect("应命中第二个 foo");
    assert_eq!((second.index, second.total), (2, 3), "第二个命中应为 2/3: {second:?}");
    let third = handle
        .search_with("foo", true, opts)
        .expect("valid plain query")
        .expect("应命中第三个 foo");
    assert_eq!((third.index, third.total), (3, 3), "第三个命中应为 3/3: {third:?}");
    let wrapped = handle
        .search_with("foo", true, opts)
        .expect("valid plain query")
        .expect("继续向前应绕回首个 foo");
    assert_eq!((wrapped.index, wrapped.total), (1, 3), "绕回后应为 1/3: {wrapped:?}");

    // 反向：从 1/3 往回走应回到 3/3。
    let back = handle
        .search_with("foo", false, opts)
        .expect("valid plain query")
        .expect("Shift+Enter 应命中上一个");
    assert_eq!((back.index, back.total), (3, 3), "反向应回到 3/3: {back:?}");

    // 无命中时 UI 显示 0/0；core 返回 None（由 UI 层格式化）。
    assert!(handle.search("no-such-token", true).is_none());

    // 同行多次匹配也要逐个计数："foo foo" 4 个 o，"bar foo" 2 个 o。
    handle.search_clear();
    let o = handle
        .search_with("o", true, opts)
        .expect("valid plain query")
        .expect("应命中 o");
    assert_eq!(o.total, 6, "o 的总数应为 6: {o:?}");
    handle.search_clear();
}

#[test]
fn read_only_blocks_input_and_restores() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 8,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "cat; sleep 2".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");
    // 等 shell 起来并建立 tty 行编辑。
    std::thread::sleep(Duration::from_millis(150));

    assert!(!handle.read_only());
    handle.set_read_only(true);
    assert!(handle.read_only());
    assert!(!handle.try_input(b"nope\n"), "Read Only 时 try_input 必须拒绝");
    handle.input(b"blocked\n");
    std::thread::sleep(Duration::from_millis(150));
    let frame = handle.pull(handle.new_frame());
    assert!(!frame_text(&frame).contains("blocked"), "Read Only 不能写 PTY");

    handle.set_read_only(false);
    assert!(!handle.read_only());
    handle.input(b"allowed\n");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("allowed") {
            break;
        }
        assert!(Instant::now() < deadline, "关闭 Read Only 后输入应恢复");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn reset_clears_state_and_contents_include_scrollback() {
    let handle = spawn(PaneConfig {
        cols: 40,
        rows: 8,
        scrollback: 100,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), "seq 1 40; cat; sleep 2".to_string()],
        ..PaneConfig::default()
    })
    .expect("spawn");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("40") {
            break;
        }
        assert!(Instant::now() < deadline, "没有等到输出");
        std::thread::sleep(Duration::from_millis(5));
    }

    // Save Contents 路径：contents() 要包含回滚历史，而不只是当前视口。
    let contents = handle.contents().expect("contents 应有回复");
    assert!(contents.starts_with("1\n"), "contents 应从最旧历史开始: {contents:?}");
    assert!(contents.contains("40\n"), "contents 应包含末尾输出: {contents:?}");

    // 先留一个搜索高亮，Reset 必须一并清掉。
    let hit = handle.search("40", true).expect("应能找到 40");
    assert!(hit.line < 8);
    handle.reset();
    let frame = handle.pull(handle.new_frame());
    assert!(frame_text(&frame).trim().is_empty(), "Reset 后屏幕应为空: {frame:?}");
    assert!(frame.search.is_none(), "Reset 应清除搜索高亮");
    assert_eq!(handle.scroll_position().history, 0, "Reset 应清空回滚");
    assert!(handle.contents().expect("contents 应有回复").is_empty());

    // Reset 后输入仍可正常写入；再验证 Reset and Clear。
    handle.input(b"after-reset\n");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut frame = handle.new_frame();
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains("after-reset") {
            break;
        }
        assert!(Instant::now() < deadline, "Reset 后输入应恢复");
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.reset_and_clear();
    let frame = handle.pull(handle.new_frame());
    assert!(frame_text(&frame).trim().is_empty(), "Reset and Clear 后屏幕应为空");
    assert_eq!(handle.scroll_position().history, 0);
}
