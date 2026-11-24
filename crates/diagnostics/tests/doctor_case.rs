// SPDX-License-Identifier: GPL-3.0-only
//! End-to-end tests for the deterministic doctor library.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use xtervo_diagnostics::doctor::{
    diagnostic_case_json_schema, DiagnosticCase, Doctor, DoctorConfig, DoctorInput, Environment,
    EvidenceKind, EvidenceLimits, FrameSummary, GpuInfo, MetricsSummary, RenderProfileSummary,
    RendererInfo, Symptom,
};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "xtervo-doctor-integration-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, content).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn base_env(dir: &TempDir, config: &Path) -> Environment {
    Environment {
        source: Some("integration-test".to_string()),
        version: Some("0.3.1".to_string()),
        commit: Some("a6afbd1d912f".to_string()),
        dirty: Some(false),
        build_time: Some("2026-11-11T18:00:00Z".to_string()),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        os: Some("linux".to_string()),
        config_path: Some(config.display().to_string()),
        profiles_path: Some(dir.join("profiles.toml").display().to_string()),
        log_path: None,
        log_dir: Some(dir.join("logs").display().to_string()),
        ..Environment::default()
    }
}

fn sample_frame() -> FrameSummary {
    FrameSummary::from_diag_json(&json!({
        "schema": "xtervo.diag.v1",
        "kind": "snapshot",
        "cols": 80,
        "rows": 4,
        "generation": 3,
        "cursor": {"row": 1, "col": 2, "visible": true, "shape": "block"},
        "damage": {
            "full": false,
            "region_count": 1,
            "rects": [{"x": 0, "y": 1, "w": 20, "h": 1}]
        },
        "images": [],
        "text": ["hello", "world", "tail", ""]
    }))
    .expect("frame")
}

fn placeholder_metrics() -> MetricsSummary {
    MetricsSummary::from_diag_json(&json!({
        "schema": "xtervo.diag.v1",
        "kind": "metrics",
        "initialized": false,
        "source": "cli",
        "metrics": {
            "frame_count": 0,
            "frame_time": {"p95_us": null, "samples": 0, "p95_overflow": false},
            "damage": {"area_ratio_last_ppm": 0, "area_ratio_max_ppm": 0},
            "stale_frames": 0,
            "textures": {"live_bytes_estimate": 0},
            "images": {"placements": 0}
        }
    }))
    .expect("metrics")
}

fn run_case(input: &DoctorInput) -> DiagnosticCase {
    Doctor::default().run(input).expect("doctor run")
}

#[test]
fn schema_roundtrip_is_deterministic() {
    let dir = TempDir::new("roundtrip");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let frame = sample_frame();
    let profile = RenderProfileSummary::from_text(
        "test-profile",
        "frame_ms=12.5\ndraw_ms=3.0\nframe_ms=20.0\n",
        Some(true),
        None,
    );
    let input = DoctorInput {
        symptom: Symptom::reported("滚动后残影"),
        environment: base_env(&dir, &config),
        frame: Some(frame),
        metrics: Some(placeholder_metrics()),
        render_profile: Some(profile),
        attachments: vec![],
    };
    let case = run_case(&input);
    let second = run_case(&input);
    assert_eq!(case, second, "same input must produce the same case");

    let text = case.to_json_pretty().expect("serialize");
    let parsed = DiagnosticCase::from_json(&text).expect("parse and validate");
    assert_eq!(parsed, case);
    assert_eq!(parsed.to_json_pretty().expect("reserialize"), text);

    let value: Value = serde_json::from_str(&text).unwrap();
    let schema = diagnostic_case_json_schema();
    for required in schema["required"].as_array().unwrap() {
        let key = required.as_str().unwrap();
        assert!(value.get(key).is_some(), "missing top-level key {key}");
    }
    assert_eq!(value["schema"], "xtervo.diagnostic_case.v1");
    assert!(value["case_id"].as_str().unwrap().starts_with("case-"));
    assert!(!value["findings"].as_array().unwrap().is_empty());
}

#[test]
fn embedded_schema_file_matches_generator() {
    let embedded: Value =
        serde_json::from_str(include_str!("../schema/xtervo.diagnostic_case.v1.json"))
            .expect("embedded schema is JSON");
    assert_eq!(embedded, diagnostic_case_json_schema());
}

#[test]
fn secrets_are_redacted_from_both_case_and_issue() {
    let dir = TempDir::new("redaction");
    let config = dir.write(
        "config.toml",
        "theme = \"dark\"\napi_key = \"sk-live-1234567890\"\npassword = \"hunter2\"\n",
    );
    let input = DoctorInput {
        symptom: Symptom::reported(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123",
        )
        .with_details("api_key=sk-live-in-symptom"),
        environment: base_env(&dir, &config),
        frame: None,
        metrics: None,
        render_profile: None,
        attachments: vec![xtervo_diagnostics::doctor::Attachment {
            label: "log".to_string(),
            content: "fetch https://alice:s3cr3t@example.com/x?token=tok123\n\
                      token=ghp_abcdefghijklmnopqrstuvwxyz0123456789\n\
                      AKIAABCDEFGHIJKLMNOP\n"
                .to_string(),
            kind: EvidenceKind::Logs,
        }],
    };
    let case = run_case(&input);
    case.evidence.verify().expect("evidence verifies");
    assert!(case.evidence.redaction.matches > 0);
    let case_text = case.to_json_pretty().unwrap();
    let issue = &case.issue_draft.markdown;
    for secret in [
        "sk-live-1234567890",
        "hunter2",
        "eyJhbGciOiJIUzI1NiJ9",
        "s3cr3t",
        "tok123",
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "AKIAABCDEFGHIJKLMNOP",
    ] {
        assert!(!case_text.contains(secret), "case leaked {secret}");
        assert!(!issue.contains(secret), "issue leaked {secret}");
    }
    assert!(case
        .findings
        .iter()
        .any(|finding| finding.id == "finding.config.secrets"));
}

#[test]
fn issue_template_has_required_sections_and_metadata() {
    let dir = TempDir::new("issue");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let input = DoctorInput {
        symptom: Symptom::reported("GPU 打开后更慢").with_repro_steps(["打开图片", "滚动三次"]),
        environment: Environment {
            renderer: Some(RendererInfo {
                requested: Some("wgpu".to_string()),
                active: Some("skia".to_string()),
                fallback: Some(true),
                fallback_reason: Some("adapter init failed".to_string()),
                gpu: Some(GpuInfo {
                    adapter: Some("llvmpipe".to_string()),
                    ..GpuInfo::default()
                }),
            }),
            ..base_env(&dir, &config)
        },
        frame: Some(sample_frame()),
        metrics: Some(placeholder_metrics()),
        render_profile: None,
        attachments: vec![],
    };
    let case = run_case(&input);
    let markdown = &case.issue_draft.markdown;
    let headings = [
        "## 现象",
        "## 环境",
        "## 复现",
        "## 证据",
        "## 第一处分歧",
        "## 已排除",
        "## 建议",
        "## 回归测试",
    ];
    let positions: Vec<usize> = headings
        .iter()
        .map(|heading| {
            markdown
                .find(heading)
                .unwrap_or_else(|| panic!("missing {heading}"))
        })
        .collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(markdown.contains("GPU adapter"));
    assert!(markdown.contains(&case.evidence.sha256));
    assert!(markdown.contains("## 第一处分歧"));
    assert!(markdown.contains("## 回归测试"));
    assert!(case.issue_draft.title.starts_with("[Doctor]"));
    assert!(case
        .issue_draft
        .labels
        .iter()
        .any(|label| label == "layer:renderer"));
    assert!(case.first_divergence().is_some());
}

#[test]
fn every_finding_has_existing_evidence() {
    let dir = TempDir::new("evidence-refs");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let input = DoctorInput {
        symptom: Symptom::default(),
        environment: Environment {
            renderer: Some(RendererInfo {
                requested: Some("wgpu".to_string()),
                active: Some("software".to_string()),
                fallback: Some(true),
                fallback_reason: None,
                gpu: Some(GpuInfo {
                    adapter: Some("llvmpipe".to_string()),
                    ..GpuInfo::default()
                }),
            }),
            ..base_env(&dir, &config)
        },
        ..DoctorInput::default()
    };
    let case = run_case(&input);
    let evidence_ids: std::collections::BTreeSet<&str> = case
        .evidence
        .items
        .iter()
        .map(|item| item.id.as_str())
        .collect();
    for finding in &case.findings {
        assert!(
            !finding.evidence.is_empty(),
            "finding {} has no evidence",
            finding.id
        );
        for reference in &finding.evidence {
            assert!(evidence_ids.contains(reference.as_str()));
        }
        assert!(case.probes.iter().any(|probe| probe.id == finding.probe));
    }
    for probe in &case.probes {
        assert!(
            !probe.evidence.is_empty(),
            "probe {} has no evidence",
            probe.id
        );
        for reference in &probe.evidence {
            assert!(evidence_ids.contains(reference.as_str()));
        }
    }
}

#[test]
fn missing_data_stays_missing_and_is_not_a_healthy_zero() {
    let case = run_case(&DoctorInput::default());
    let value: Value = serde_json::from_str(&case.to_json_pretty().unwrap()).unwrap();
    assert!(value["environment"]["commit"].is_null());
    assert!(value["environment"]["renderer"].is_null());
    assert!(value["environment"]["config_path"].is_null());

    let kinds: Vec<EvidenceKind> = case.evidence.items.iter().map(|item| item.kind).collect();
    assert!(!kinds.contains(&EvidenceKind::Frame));
    assert!(!kinds.contains(&EvidenceKind::Metrics));
    assert!(!kinds.contains(&EvidenceKind::RenderProfile));

    for probe in [
        "probe.frame",
        "probe.damage",
        "probe.metrics",
        "probe.render_profile",
    ] {
        let probe = case
            .probes
            .iter()
            .find(|candidate| candidate.id == probe)
            .unwrap_or_else(|| panic!("missing {probe}"));
        assert!(
            matches!(
                probe.status,
                xtervo_diagnostics::doctor::ProbeStatus::Unknown
                    | xtervo_diagnostics::doctor::ProbeStatus::Skipped
            ),
            "{} should be unknown/skipped, got {:?}",
            probe.id,
            probe.status
        );
    }
    let markdown = &case.issue_draft.markdown;
    assert!(markdown.contains("未收集"));
    assert!(!markdown.contains("0ms"));
    assert!(!markdown.contains("0.0ms"));
    assert!(!markdown.contains("0.0%"));
    case.validate().expect("case validates");
}

#[test]
fn evidence_caps_are_enforced_and_still_validate() {
    let dir = TempDir::new("caps");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let input = DoctorInput {
        symptom: Symptom::reported("大附件"),
        environment: base_env(&dir, &config),
        attachments: vec![xtervo_diagnostics::doctor::Attachment {
            label: "huge log".to_string(),
            content: "x".repeat(100_000),
            kind: EvidenceKind::Logs,
        }],
        ..DoctorInput::default()
    };
    let config = DoctorConfig {
        evidence_limits: EvidenceLimits {
            max_item_bytes: 128,
            max_total_bytes: 256,
            max_items: 2,
        },
        ..DoctorConfig::default()
    };
    let case = Doctor::new(config).run(&input).expect("run with caps");
    assert!(case.evidence.items.len() <= 2);
    assert!(case.evidence.total_bytes <= 256);
    assert!(case.evidence.verify().is_ok());
    assert!(case
        .evidence
        .items
        .iter()
        .all(|item| item.content.len() <= 128));
    assert!(
        case.evidence.items.iter().any(|item| item.truncated)
            || !case.evidence.omissions.is_empty()
    );
    assert!(case
        .findings
        .iter()
        .any(|finding| finding.id == "finding.evidence.limited"));
    case.validate().expect("capped case validates");
}

#[test]
fn wrong_path_kind_is_reported_as_a_file_failure() {
    let dir = TempDir::new("path-kind");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let profiles_dir = dir.join("profiles-as-dir");
    fs::create_dir_all(&profiles_dir).unwrap();
    let log = dir.write("xtervo.log", "ready\n");
    let input = DoctorInput {
        environment: Environment {
            profiles_path: Some(profiles_dir.display().to_string()),
            log_path: Some(log.display().to_string()),
            ..base_env(&dir, &config)
        },
        ..DoctorInput::default()
    };
    let case = run_case(&input);
    let probe = case
        .probes
        .iter()
        .find(|probe| probe.id == "probe.paths")
        .unwrap();
    assert_eq!(probe.status, xtervo_diagnostics::doctor::ProbeStatus::Fail);
    assert!(case
        .findings
        .iter()
        .any(|finding| finding.id == "finding.paths.unreadable"));
}

#[test]
fn readable_log_path_is_checked_without_reading_it_into_the_case() {
    let dir = TempDir::new("log-readable");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let log = dir.write("xtervo.log", "api_key=sk-live-log-secret\nready\n");
    let input = DoctorInput {
        environment: Environment {
            log_path: Some(log.display().to_string()),
            ..base_env(&dir, &config)
        },
        ..DoctorInput::default()
    };
    let case = run_case(&input);
    let probe = case
        .probes
        .iter()
        .find(|probe| probe.id == "probe.paths")
        .unwrap();
    assert_eq!(probe.status, xtervo_diagnostics::doctor::ProbeStatus::Pass);
    assert!(!case
        .to_json_pretty()
        .unwrap()
        .contains("sk-live-log-secret"));
}

#[test]
fn config_secret_is_reported_but_never_stored() {
    let dir = TempDir::new("config-secret");
    let config = dir.write(
        "config.toml",
        "theme = \"dark\"\ntoken = \"ghp_abcdefghijklmnopqrstuvwxyz0123456789\"\n",
    );
    let input = DoctorInput {
        environment: base_env(&dir, &config),
        ..DoctorInput::default()
    };
    let case = run_case(&input);
    let text = case.to_json_pretty().unwrap();
    assert!(!text.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));
    assert!(case
        .findings
        .iter()
        .any(|finding| finding.id == "finding.config.secrets"));
    let finding = case
        .findings
        .iter()
        .find(|finding| finding.id == "finding.config.secrets")
        .unwrap();
    assert_eq!(finding.severity, xtervo_diagnostics::doctor::Severity::Error);
}
