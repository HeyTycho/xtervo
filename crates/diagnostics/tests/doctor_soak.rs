// SPDX-License-Identifier: GPL-3.0-only
//! Optional 2-minute memory gate for the deterministic doctor.
//!
//! This does not exercise the app render loop (the slice deliberately does not
//! wire doctor into it). It repeatedly builds cases with a bounded viewport,
//! redacted attachment, placeholder metrics and profile summary, then checks
//! that RSS/threads/FDs stay bounded. Run explicitly:
//!
//! `cargo test -p xtervo-diagnostics --test doctor_soak -- --ignored --nocapture`

#![cfg(target_os = "linux")]

use std::fs;
use std::time::{Duration, Instant};

use serde_json::json;
use xtervo_diagnostics::doctor::{
    Attachment, Doctor, DoctorInput, Environment, EvidenceKind, FrameSummary, MetricsSummary,
    RenderProfileSummary, Symptom,
};

#[derive(Clone, Copy, Debug)]
struct ProcStats {
    rss_kib: u64,
    threads: u64,
    fds: usize,
}

fn proc_stats() -> ProcStats {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut rss_kib = 0;
    let mut threads = 0;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss_kib = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
        }
        if let Some(value) = line.strip_prefix("Threads:") {
            threads = value.trim().parse().unwrap_or(0);
        }
    }
    let fds = fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .unwrap_or(0);
    ProcStats {
        rss_kib,
        threads,
        fds,
    }
}

fn fixture() -> DoctorInput {
    let frame = FrameSummary::from_diag_json(&json!({
        "schema": "xtervo.diag.v1",
        "kind": "snapshot",
        "cols": 160,
        "rows": 40,
        "generation": 9,
        "damage": {
            "full": false,
            "region_count": 3,
            "rects": [
                {"x": 0, "y": 3, "w": 40, "h": 1},
                {"x": 10, "y": 7, "w": 80, "h": 1},
                {"x": 0, "y": 39, "w": 160, "h": 1}
            ]
        },
        "images": [],
        "text": (0..40).map(|row| format!("row {row} api_key=sk-live-{row:04}")).collect::<Vec<_>>()
    }))
    .expect("frame");
    let metrics = MetricsSummary::from_diag_json(&json!({
        "schema": "xtervo.diag.v1",
        "kind": "metrics",
        "initialized": false,
        "source": "cli",
        "metrics": {
            "frame_count": 0,
            "frame_time": {"p95_us": null, "samples": 0},
            "damage": {"area_ratio_max_ppm": 0},
            "stale_frames": 0
        }
    }))
    .expect("metrics");
    let profile = RenderProfileSummary::from_text(
        "soak-profile.log",
        "frame_ms=12.5 draw_ms=3.0 upload_ms=1.25\nframe_ms=18.0\n",
        Some(true),
        None,
    );
    DoctorInput {
        symptom: Symptom::reported("soak: 滚动后残影").with_repro_steps(["scroll", "resize"]),
        environment: Environment {
            source: Some("soak".to_string()),
            version: Some("0.3.1".to_string()),
            commit: Some("soak".to_string()),
            dirty: Some(false),
            build_time: Some("2026-11-11T18:00:00Z".to_string()),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            ..Environment::default()
        },
        frame: Some(frame),
        metrics: Some(metrics),
        render_profile: Some(profile),
        attachments: vec![Attachment {
            label: "bounded log".to_string(),
            content: format!(
                "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789\n{}",
                "S".repeat(8 * 1024)
            ),
            kind: EvidenceKind::Logs,
        }],
    }
}

#[test]
#[ignore = "2-minute memory gate; run explicitly with --ignored"]
fn two_minute_doctor_memory_gate() {
    let input = fixture();
    let doctor = Doctor::default();
    for _ in 0..50 {
        std::hint::black_box(doctor.run(&input).expect("warmup").case_id.len());
    }

    let baseline = proc_stats();
    let start = Instant::now();
    let mut iterations: u64 = 0;
    while start.elapsed() < Duration::from_secs(120) {
        let case = doctor.run(&input).expect("soak run");
        if iterations % 1_000 == 0 {
            case.validate().expect("soak case validates");
        }
        std::hint::black_box(case.case_id.len());
        iterations = iterations.saturating_add(1);
    }
    let end = proc_stats();
    eprintln!(
        "doctor soak: iterations={iterations} elapsed={:?} baseline={baseline:?} end={end:?}",
        start.elapsed()
    );

    assert!(iterations > 0, "no iterations completed");
    assert!(
        end.rss_kib <= baseline.rss_kib.saturating_add(32 * 1024),
        "RSS grew too much: baseline={} KiB end={} KiB",
        baseline.rss_kib,
        end.rss_kib
    );
    assert!(
        end.threads <= baseline.threads.saturating_add(2),
        "threads grew: baseline={} end={}",
        baseline.threads,
        end.threads
    );
    assert!(
        end.fds <= baseline.fds.saturating_add(4),
        "FDs grew: baseline={} end={}",
        baseline.fds,
        end.fds
    );
}
