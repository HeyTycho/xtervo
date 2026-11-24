// SPDX-License-Identifier: GPL-3.0-only
//! P0 手工探针：慢 reader（每 10ms 读 1 字节）持续灌入 2000×1KiB 输入时，
//! `PaneHandle::input()` 与 `pull()` 是否仍有界。打印机器可读结果；用 2s
//! watchdog 退出，避免被卡住的线程拖死进程。
//!
//! 运行：
//! `cargo run -p xtervo-core --example p0_blocking_probe`
//!
//! 修复前 master（a82ac52）的同一探针：input/pull 均 >2s 不返回；
//! 修复后：input ~1ms、pull <1ms。详见 docs/self-iteration.md 的 P0 记录。

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use xtervo_core::{spawn, PaneConfig};

fn main() {
    let handle = Arc::new(
        spawn(PaneConfig {
            cols: 100,
            rows: 30,
            scrollback: 100,
            shell: Some("/bin/sh".to_string()),
            args: vec![
                "-c".to_string(),
                "stty raw -echo; while true; do dd bs=1 count=1 of=/dev/null 2>/dev/null; sleep 0.01; done"
                    .to_string(),
            ],
            ..PaneConfig::default()
        })
        .expect("spawn pane"),
    );

    let (tx, rx) = mpsc::channel::<String>();
    let input_handle = handle.clone();
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        let _ = input_tx.send("input_starting".to_string());
        let chunk = vec![b'x'; 1024];
        let started = Instant::now();
        let mut worst = Duration::ZERO;
        for _ in 0..2000 {
            let t = Instant::now();
            input_handle.input(&chunk);
            worst = worst.max(t.elapsed());
        }
        let _ = tx.send(format!(
            "input_done elapsed_ms={:.1} worst_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0,
            worst.as_secs_f64() * 1000.0
        ));
    });

    // 输入线程开始灌入后等 1s，让写 backlog 真正形成背压。
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(line) => println!("{line}"),
        Err(_) => {
            println!("input_not_started=true");
            std::process::exit(0);
        }
    }
    std::thread::sleep(Duration::from_secs(1));

    let (pull_tx, pull_rx) = mpsc::channel::<String>();
    let pull_handle = handle.clone();
    std::thread::spawn(move || {
        let t = Instant::now();
        let frame = pull_handle.pull(pull_handle.new_frame());
        let _ = pull_tx.send(format!(
            "pull_done elapsed_ms={:.1} rows={}",
            t.elapsed().as_secs_f64() * 1000.0,
            frame.rows
        ));
    });

    match pull_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(line) => println!("{line}"),
        Err(_) => println!("pull_blocked=true (no frame within 2000ms)"),
    }
    match rx.recv_timeout(Duration::from_millis(100)) {
        Ok(line) => println!("{line}"),
        Err(_) => println!("input_blocked=true (2000 chunks did not return within 2100ms)"),
    }
}
