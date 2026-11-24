// SPDX-License-Identifier: GPL-3.0-only
//! Phase 0 性能基准。
//!
//! 运行：`./scripts/bench.sh`（或 `cargo bench -p xtervo-core`）
//!
//! 覆盖三组：
//! 1. `parse/*`  —— VT 解析吞吐（纯 CPU，不含 PTY）
//! 2. `frame_pull/*` —— 拉取式快照成本（整屏 vs 脏行）
//! 3. `e2e/*` —— 真实 PTY 会话端到端（spawn → 屏幕可见）
//!
//! 洪峰压力脚本见 `scripts/stress.py`。

use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use xtervo_core::{spawn, CoreEvent, Frame, PaneConfig, TermSim};

const MIB: usize = 1024 * 1024;
const EOF_MARKER: &str = "XTERVO-EOF-MARKER";

fn plain_payload(size: usize) -> Vec<u8> {
    let line = b"The quick brown fox jumps over the lazy dog 0123456789\r\n";
    let mut v = Vec::with_capacity(size + line.len());
    while v.len() < size {
        v.extend_from_slice(line);
    }
    v
}

fn sgr_payload(size: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(size + 128);
    let mut i = 0u32;
    while v.len() < size {
        v.extend_from_slice(b"\x1b[38;5;");
        v.extend_from_slice((i % 256).to_string().as_bytes());
        v.extend_from_slice(b"mcolorful text\x1b[0m ");
        i += 1;
        if i % 4 == 0 {
            v.extend_from_slice(b"\r\n");
        }
    }
    v
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

// ---------------------------------------------------------------- parse

fn bench_parse(c: &mut Criterion) {
    let data = plain_payload(10 * MIB);
    let sgr = sgr_payload(10 * MIB);

    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.sample_size(20);
    group.bench_function("plain_10mb", |b| {
        let mut sim = TermSim::new(200, 50, 2000);
        b.iter(|| {
            sim.advance(black_box(&data));
            black_box(sim.pull().copied_cells);
        })
    });

    group.throughput(Throughput::Bytes(sgr.len() as u64));
    group.bench_function("sgr_10mb", |b| {
        let mut sim = TermSim::new(200, 50, 2000);
        b.iter(|| {
            sim.advance(black_box(&sgr));
            black_box(sim.pull().copied_cells);
        })
    });
    group.finish();
}

// ------------------------------------------------------------ frame pull

fn bench_frame_pull(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_pull");
    group.sample_size(50);

    group.bench_function("full_200x50", |b| {
        b.iter_batched(
            || {
                let mut sim = TermSim::new(200, 50, 2000);
                // 交替尺寸强制 full damage
                sim.resize(201, 50);
                sim
            },
            |mut sim| black_box(sim.pull().copied_cells),
            BatchSize::SmallInput,
        )
    });

    group.bench_function("dirty_single_line", |b| {
        let mut sim = TermSim::new(200, 50, 2000);
        sim.advance(b"seed\r\n");
        let _ = sim.pull();
        let mut i = 0u32;
        b.iter(|| {
            sim.advance(format!("line {i}\r\n").as_bytes());
            i += 1;
            black_box(sim.pull().copied_cells)
        })
    });
    group.finish();
}

// ------------------------------------------------------------------ e2e

fn make_cat_file() -> PathBuf {
    let path = std::env::temp_dir().join("xtervo-cat-10mb.txt");
    if let Ok(md) = std::fs::metadata(&path) {
        if md.len() > 10 * MIB as u64 {
            return path;
        }
    }
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).expect("create payload"));
    let payload = plain_payload(10 * MIB);
    f.write_all(&payload).unwrap();
    f.write_all(b"\r\n").unwrap();
    f.write_all(EOF_MARKER.as_bytes()).unwrap();
    f.write_all(b"\r\n").unwrap();
    f.flush().unwrap();
    path
}

/// 端到端：spawn `cat 10MB` → 直到末行在网格中可见。
fn run_cat_session(path: &PathBuf) -> Duration {
    let start = Instant::now();
    let handle = spawn(PaneConfig {
        cols: 200,
        rows: 50,
        scrollback: 2000,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), format!("cat {}", path.display())],
        ..PaneConfig::default()
    })
    .expect("spawn cat pane");

    let mut frame = handle.new_frame();
    let deadline = start + Duration::from_secs(30);
    let mut exited = false;
    loop {
        frame = handle.pull(frame);
        if frame_text(&frame).contains(EOF_MARKER) {
            break;
        }
        while let Some(ev) = handle.try_event() {
            if ev == CoreEvent::Exited {
                exited = true;
            }
        }
        if exited || Instant::now() > deadline {
            // 收尾：再拉两帧，确保 EOF 后残留数据被解析
            frame = handle.pull(frame);
            if frame_text(&frame).contains(EOF_MARKER) {
                break;
            }
            panic!("cat 未在 30s 内完成 (exited={exited})\n{}", frame_text(&frame));
        }
    }
    start.elapsed()
}

fn bench_e2e(c: &mut Criterion) {
    let path = make_cat_file();
    eprintln!("payload: {}", path.display());

    let mut group = c.benchmark_group("e2e");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.bench_function("cat_10mb_until_visible", |b| {
        b.iter(|| {
            let d = run_cat_session(&path);
            eprintln!("  cat 10MB 端到端: {:?}", d);
            black_box(d)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_parse, bench_frame_pull, bench_e2e);
criterion_main!(benches);
