// SPDX-License-Identifier: GPL-3.0-only
//! Deterministic health checks and case assembly.
//!
//! Every probe runs in a fixed order and every finding carries at least one
//! evidence reference. Missing inputs produce `Unknown`/`Skipped` and an
//! explicit "未收集" message; they are never promoted to PASS and never filled
//! with plausible measurements.
//!
//! Checks only inspect files/paths and already-collected in-memory summaries.
//! They do not start a window, spawn a PTY, read scrollback, invoke wgpu, or
//! contact the network.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;

use serde::Serialize;
use serde_json::{json, Value};

use super::evidence::{EvidenceBundleBuilder, EvidenceKind, EvidenceLimits, EvidenceStats};
use super::ingest::{FrameSummary, MetricsSummary, RenderProfileSummary};
use super::model::{
    Confidence, DiagnosticCase, Environment, Finding, Layer, Probe, ProbeStatus, Recommendation,
    RecommendationKind, RegressionStatus, RegressionTest, Severity, Symptom, ValidationError,
};
use super::redact::redact_text;

const MAX_CONFIG_SCAN_BYTES: usize = 256 * 1024;
const MAX_PATH_ERROR_CHARS: usize = 300;

/// Tunable deterministic thresholds. The defaults are intentionally
/// conservative; all are surfaced in the case so a reader can see why a probe
/// warned.
#[derive(Clone, Debug, PartialEq)]
pub struct DoctorConfig {
    pub evidence_limits: EvidenceLimits,
    /// Frame-time p95 above this is a warning. 33 ms is two 60 Hz frames.
    pub slow_frame_p95_us: u64,
    /// Render-profile stage maximum above this (milliseconds) is a warning.
    pub slow_stage_ms: f64,
    /// Damage coverage (ppm) above this is a warning.
    pub high_damage_ppm: u64,
    pub max_config_scan_bytes: usize,
}

impl Default for DoctorConfig {
    fn default() -> Self {
        Self {
            evidence_limits: EvidenceLimits::default(),
            slow_frame_p95_us: 33_000,
            slow_stage_ms: 33.0,
            high_damage_ppm: 900_000,
            max_config_scan_bytes: MAX_CONFIG_SCAN_BYTES,
        }
    }
}

/// Explicit, bounded attachment. The caller decides to include it; the doctor
/// redacts and caps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub label: String,
    pub content: String,
    pub kind: EvidenceKind,
}

/// All inputs to a deterministic doctor run. Data that was not collected stays
/// `None`.
#[derive(Clone, Debug, Default)]
pub struct DoctorInput {
    pub symptom: Symptom,
    pub environment: Environment,
    pub frame: Option<FrameSummary>,
    pub metrics: Option<MetricsSummary>,
    pub render_profile: Option<RenderProfileSummary>,
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DoctorError {
    NoEvidence,
    Validation(ValidationError),
}

impl std::fmt::Display for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEvidence => f.write_str("无法生成任何证据（上限过小或输入为空）"),
            Self::Validation(error) => write!(f, "DiagnosticCase 校验失败: {error}"),
        }
    }
}

impl std::error::Error for DoctorError {}

impl From<ValidationError> for DoctorError {
    fn from(error: ValidationError) -> Self {
        Self::Validation(error)
    }
}

/// Stateless doctor: configuration in, case out. The same input and config
/// produce the same case id, evidence hash and markdown.
#[derive(Clone, Debug, Default)]
pub struct Doctor {
    config: DoctorConfig,
}

impl Doctor {
    pub fn new(config: DoctorConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DoctorConfig {
        &self.config
    }

    pub fn run(&self, input: &DoctorInput) -> Result<DiagnosticCase, DoctorError> {
        let (bundle, ids, inspections, config_inspection) = prepare_evidence(input, &self.config);
        let mut checks = Checks {
            input,
            config: &self.config,
            ids,
            inspections,
            config_inspection,
            probes: Vec::new(),
            findings: Vec::new(),
            recommendations: Vec::new(),
        };
        checks.run_all();
        checks.check_evidence(bundle.stats());
        if checks.findings.is_empty() || checks.probes.is_empty() {
            return Err(DoctorError::NoEvidence);
        }
        let regression_test = checks.regression_test();
        let environment = checks.input.environment.clone();
        let symptom = checks.input.symptom.clone();
        let probes = std::mem::take(&mut checks.probes);
        let findings = std::mem::take(&mut checks.findings);
        let recommendations = std::mem::take(&mut checks.recommendations);
        drop(checks);
        if bundle.stats().item_count == 0 {
            return Err(DoctorError::NoEvidence);
        }
        let evidence = bundle.finish();
        DiagnosticCase::assemble(
            symptom,
            environment,
            evidence,
            probes,
            findings,
            recommendations,
            regression_test,
        )
        .map_err(DoctorError::Validation)
    }
}

#[derive(Debug, Default)]
struct EvidenceIds {
    environment: Option<String>,
    paths: Option<String>,
    config: Option<String>,
    frame: Option<String>,
    damage: Option<String>,
    metrics: Option<String>,
    render_profile: Option<String>,
    attachments: Vec<String>,
}

fn prepare_evidence(
    input: &DoctorInput,
    config: &DoctorConfig,
) -> (
    EvidenceBundleBuilder,
    EvidenceIds,
    Vec<PathInspection>,
    Option<ConfigInspection>,
) {
    let mut builder = EvidenceBundleBuilder::new(config.evidence_limits);
    let mut ids = EvidenceIds::default();

    let environment = serde_json::to_value(&input.environment).unwrap_or(Value::Null);
    ids.environment = builder.add_json(EvidenceKind::Environment, "environment", &environment);

    let mut inspections = Vec::new();
    for (name, path) in [
        ("config", input.environment.config_path.as_deref()),
        ("profiles", input.environment.profiles_path.as_deref()),
        ("log", input.environment.log_path.as_deref()),
        ("log_dir", input.environment.log_dir.as_deref()),
    ] {
        if let Some(inspection) = inspect_path(name, path) {
            inspections.push(inspection);
        }
    }
    let paths_value = json!({
        "source": input.environment.source,
        "checked": inspections.len(),
        "paths": inspections,
    });
    ids.paths = builder.add_json(EvidenceKind::Paths, "paths", &paths_value);

    let config_inspection = inspect_config(
        input.environment.config_path.as_deref(),
        config.max_config_scan_bytes,
    );
    let config_value = match &config_inspection {
        Some(inspection) => serde_json::to_value(inspection).unwrap_or(Value::Null),
        None => json!({"checked": false, "reason": "config 路径未提供"}),
    };
    ids.config = builder.add_json(EvidenceKind::Config, "config", &config_value);

    if let Some(frame) = &input.frame {
        let value = serde_json::to_value(frame).unwrap_or(Value::Null);
        ids.frame = builder.add_json(EvidenceKind::Frame, "frame", &value);
        if let Some(damage) = &frame.damage {
            let value = serde_json::to_value(damage).unwrap_or(Value::Null);
            ids.damage = builder.add_json(EvidenceKind::Damage, "damage", &value);
        }
    }
    if let Some(metrics) = &input.metrics {
        let value = serde_json::to_value(metrics).unwrap_or(Value::Null);
        ids.metrics = builder.add_json(EvidenceKind::Metrics, "metrics", &value);
    }
    if let Some(profile) = &input.render_profile {
        let value = serde_json::to_value(profile).unwrap_or(Value::Null);
        ids.render_profile =
            builder.add_json(EvidenceKind::RenderProfile, "render_profile", &value);
    }
    for attachment in &input.attachments {
        if let Some(id) = builder.add_text(attachment.kind, &attachment.label, &attachment.content)
        {
            ids.attachments.push(id);
        }
    }
    (builder, ids, inspections, config_inspection)
}

#[derive(Clone, Debug, Serialize)]
struct PathInspection {
    name: String,
    path: String,
    exists: Option<bool>,
    is_file: Option<bool>,
    is_dir: Option<bool>,
    readable: Option<bool>,
    bytes: Option<u64>,
    mode: Option<u32>,
    error: Option<String>,
}

fn inspect_path(name: &str, path: Option<&str>) -> Option<PathInspection> {
    let path = path?;
    let mut inspection = PathInspection {
        name: name.to_string(),
        path: clean_value(path),
        exists: None,
        is_file: None,
        is_dir: None,
        readable: None,
        bytes: None,
        mode: None,
        error: None,
    };
    match fs::metadata(path) {
        Ok(metadata) => {
            inspection.exists = Some(true);
            inspection.is_file = Some(metadata.is_file());
            inspection.is_dir = Some(metadata.is_dir());
            if metadata.is_file() {
                inspection.bytes = Some(metadata.len());
            }
            inspection.mode = file_mode(&metadata);
            inspection.readable = Some(if metadata.is_dir() {
                fs::read_dir(path).is_ok()
            } else {
                fs::File::open(path).is_ok()
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            inspection.exists = Some(false);
        }
        Err(error) => {
            inspection.error = Some(clean_value(&truncate_chars(
                &error.to_string(),
                MAX_PATH_ERROR_CHARS,
            )));
        }
    }
    Some(inspection)
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[derive(Clone, Debug, Serialize)]
struct ConfigInspection {
    path: PathInspection,
    readable_text: bool,
    utf8: Option<bool>,
    scanned_bytes: usize,
    scan_truncated: bool,
    secret_like_matches: u64,
    secret_rules: BTreeMap<String, u64>,
    world_accessible: Option<bool>,
    read_error: Option<String>,
}

fn inspect_config(path: Option<&str>, max_scan_bytes: usize) -> Option<ConfigInspection> {
    let path_inspection = inspect_path("config", path)?;
    let mut inspection = ConfigInspection {
        path: path_inspection,
        readable_text: false,
        utf8: None,
        scanned_bytes: 0,
        scan_truncated: false,
        secret_like_matches: 0,
        secret_rules: BTreeMap::new(),
        world_accessible: None,
        read_error: None,
    };
    if inspection.path.exists != Some(true)
        || inspection.path.is_file != Some(true)
        || inspection.path.readable != Some(true)
    {
        return Some(inspection);
    }
    inspection.world_accessible = inspection.path.mode.map(|mode| mode & 0o077 != 0);
    let Some(path) = path else {
        return Some(inspection);
    };
    let max_scan_bytes = max_scan_bytes.max(1024);
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            inspection.read_error = Some(clean_value(&truncate_chars(
                &error.to_string(),
                MAX_PATH_ERROR_CHARS,
            )));
            return Some(inspection);
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file
        .by_ref()
        .take((max_scan_bytes + 1) as u64)
        .read_to_end(&mut bytes)
    {
        inspection.read_error = Some(clean_value(&truncate_chars(
            &error.to_string(),
            MAX_PATH_ERROR_CHARS,
        )));
        return Some(inspection);
    }
    inspection.scan_truncated = bytes.len() > max_scan_bytes;
    if inspection.scan_truncated {
        bytes.truncate(max_scan_bytes);
    }
    inspection.scanned_bytes = bytes.len();
    inspection.utf8 = if inspection.scan_truncated {
        None
    } else {
        Some(std::str::from_utf8(&bytes).is_ok())
    };
    inspection.readable_text = true;
    let text = String::from_utf8_lossy(&bytes);
    let (_, report) = redact_text(&text);
    inspection.secret_like_matches = report.matches;
    inspection.secret_rules = report.rules;
    Some(inspection)
}

struct Checks<'a> {
    input: &'a DoctorInput,
    config: &'a DoctorConfig,
    ids: EvidenceIds,
    inspections: Vec<PathInspection>,
    config_inspection: Option<ConfigInspection>,
    probes: Vec<Probe>,
    findings: Vec<Finding>,
    recommendations: Vec<Recommendation>,
}

impl Checks<'_> {
    fn run_all(&mut self) {
        self.check_build();
        self.check_renderer();
        self.check_gpu();
        self.check_config();
        self.check_paths();
        self.check_frame();
        self.check_damage();
        self.check_render_profile();
        self.check_metrics();
    }

    fn refs(&self, primary: Option<&String>) -> Vec<String> {
        if let Some(id) = primary {
            return vec![id.clone()];
        }
        self.ids.environment.iter().cloned().collect()
    }

    fn multi_refs(&self, primary: &[Option<&String>]) -> Vec<String> {
        let mut refs: Vec<String> = Vec::new();
        for id in primary.iter().flatten() {
            if !refs.contains(id) {
                refs.push((*id).clone());
            }
        }
        if refs.is_empty() {
            if let Some(environment) = &self.ids.environment {
                refs.push(environment.clone());
            }
        }
        refs
    }

    fn add_probe(
        &mut self,
        id: &str,
        layer: Layer,
        status: ProbeStatus,
        summary: String,
        evidence: Vec<String>,
        command: Option<String>,
    ) {
        if evidence.is_empty() {
            return;
        }
        self.probes.push(Probe {
            id: id.to_string(),
            layer,
            status,
            summary: redact_text(&summary).0,
            evidence,
            command: command.map(|command| redact_text(&command).0),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn add_finding(
        &mut self,
        id: &str,
        probe: &str,
        severity: Severity,
        title: String,
        detail: String,
        evidence: Vec<String>,
        confidence: Confidence,
        falsify: Option<String>,
        next_probe: Option<String>,
    ) -> Option<String> {
        if evidence.is_empty() {
            return None;
        }
        self.findings.push(Finding {
            id: id.to_string(),
            probe: probe.to_string(),
            severity,
            title: redact_text(&title).0,
            detail: redact_text(&detail).0,
            evidence,
            confidence,
            falsify: falsify.map(|text| redact_text(&text).0),
            next_probe: next_probe.map(|text| redact_text(&text).0),
        });
        Some(id.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_recommendation(
        &mut self,
        id: &str,
        title: &str,
        action: String,
        kind: RecommendationKind,
        evidence: Vec<String>,
        command: Option<String>,
        finding: Option<&str>,
    ) {
        if evidence.is_empty() {
            return;
        }
        self.recommendations.push(Recommendation {
            id: id.to_string(),
            title: redact_text(title).0,
            action: redact_text(&action).0,
            kind,
            evidence,
            command: command.map(|command| redact_text(&command).0),
            related_findings: finding.map(ToString::to_string).into_iter().collect(),
        });
    }

    fn check_build(&mut self) {
        let env = self.input.environment.clone();
        let missing = [
            ("version", env.version.as_deref()),
            ("commit", env.commit.as_deref()),
            ("build_time", env.build_time.as_deref()),
            ("target", env.target.as_deref()),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            let present = value
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "unknown")
                .is_some();
            (!present).then_some(name)
        })
        .collect::<Vec<_>>();
        let evidence = self.refs(self.ids.environment.as_ref());

        if missing.is_empty() {
            let dirty = env.dirty == Some(true);
            let status = if dirty {
                ProbeStatus::Warn
            } else {
                ProbeStatus::Pass
            };
            self.add_probe(
                "probe.build",
                Layer::Build,
                status,
                format!(
                    "版本 {} / commit {}{} / build {} / {}",
                    env.version.as_deref().unwrap_or("?"),
                    env.commit.as_deref().unwrap_or("?"),
                    if dirty { " (dirty)" } else { "" },
                    env.build_time.as_deref().unwrap_or("?"),
                    env.target.as_deref().unwrap_or("?")
                ),
                evidence.clone(),
                None,
            );
            if dirty {
                let finding = self.add_finding(
                    "finding.build.dirty",
                    "probe.build",
                    Severity::Warning,
                    "工作树脏：commit 不能唯一标识代码".to_string(),
                    "environment.dirty=true；二进制/证据无法只靠 commit 复现。".to_string(),
                    evidence.clone(),
                    Confidence::Observed,
                    Some("在干净 worktree 构建后 dirty=false".to_string()),
                    None,
                );
                self.add_recommendation(
                    "rec.build.clean",
                    "用干净 commit 重新构建",
                    "提交/清理工作树后重新构建并抓取 `xtervo diag env --json`；不要在 dirty 二进制上做 before/after 对比。".to_string(),
                    RecommendationKind::CollectEvidence,
                    evidence,
                    None,
                    finding.as_deref(),
                );
            } else {
                self.add_finding(
                    "finding.build.ok",
                    "probe.build",
                    Severity::Info,
                    "构建信息完整".to_string(),
                    format!(
                        "version={} commit={} build={} target={}",
                        env.version.as_deref().unwrap_or("?"),
                        env.commit.as_deref().unwrap_or("?"),
                        env.build_time.as_deref().unwrap_or("?"),
                        env.target.as_deref().unwrap_or("?")
                    ),
                    evidence,
                    Confidence::Observed,
                    None,
                    None,
                );
            }
        } else {
            self.add_probe(
                "probe.build",
                Layer::Build,
                ProbeStatus::Unknown,
                format!("缺少构建元数据: {}", missing.join(", ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.build.missing",
                "probe.build",
                Severity::Warning,
                "构建元数据不完整，无法定位版本差异".to_string(),
                format!(
                    "未收集到: {}。缺失字段保持 null，没有用默认版本填充。",
                    missing.join(", ")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("补齐字段后 probe.build 不再为 Unknown".to_string()),
                Some("xtervo diag env --json".to_string()),
            );
            self.add_recommendation(
                "rec.build.collect",
                "采集真实构建信息",
                "从实际运行的二进制执行 `xtervo diag env --json`，把输出通过 `--diag-env` 传给 doctor。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                Some("xtervo diag env --json > /tmp/xtervo-env.json".to_string()),
                finding.as_deref(),
            );
        }
    }

    fn check_renderer(&mut self) {
        let env = self.input.environment.clone();
        let evidence = self.refs(self.ids.environment.as_ref());
        let Some(renderer) = env.renderer.as_ref() else {
            self.add_probe(
                "probe.renderer",
                Layer::Renderer,
                ProbeStatus::Unknown,
                "未报告 renderer 信息".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.renderer.missing",
                "probe.renderer",
                Severity::Warning,
                "renderer 未报告，无法判断回退".to_string(),
                "environment.renderer 为空；没有把“未知”当成“未回退”。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("environment.renderer.active/fallback 被报告".to_string()),
                Some("xtervo diag env --json".to_string()),
            );
            self.add_recommendation(
                "rec.renderer.collect",
                "采集 renderer 状态",
                "运行 `xtervo diag env --json`；若已有 live 诊断，补充 active renderer、fallback 与 GPU adapter 字段。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                Some("xtervo diag env --json".to_string()),
                finding.as_deref(),
            );
            return;
        };

        let active = renderer.active.as_deref();
        let requested = renderer.requested.as_deref();
        let fallback = renderer.fallback;
        let reason = renderer.fallback_reason.as_deref();
        if fallback == Some(true) {
            self.add_probe(
                "probe.renderer",
                Layer::Renderer,
                ProbeStatus::Fail,
                format!(
                    "renderer 回退已触发: requested={} active={}",
                    requested.unwrap_or("未收集"),
                    active.unwrap_or("未收集")
                ),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.renderer.fallback",
                "probe.renderer",
                Severity::Warning,
                "renderer 发生回退".to_string(),
                format!(
                    "requested={} active={} reason={}",
                    requested.unwrap_or("未收集"),
                    active.unwrap_or("未收集"),
                    reason.unwrap_or("未报告（不猜测原因）")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("用同一环境重启后 fallback=false".to_string()),
                Some("检查启动日志/GPU adapter".to_string()),
            );
            self.add_recommendation(
                "rec.renderer.fallback",
                "定位 renderer 回退原因",
                "抓取启动日志中的 renderer/GPU 初始化错误，并分别用显式 backend 重试；不要在原因未知时归咎于 GPU 性能。".to_string(),
                RecommendationKind::Probe,
                evidence,
                Some("XTERVO_RENDER_PROFILE=1 xtervo 2>&1 | tee /tmp/xtervo-render.log".to_string()),
                finding.as_deref(),
            );
            return;
        }
        if let (Some(active), Some(requested)) = (active, requested) {
            if active != requested {
                self.add_probe(
                    "probe.renderer",
                    Layer::Renderer,
                    ProbeStatus::Fail,
                    format!("renderer 请求与实际不一致: requested={requested} active={active}"),
                    evidence.clone(),
                    None,
                );
                let finding = self.add_finding(
                    "finding.renderer.mismatch",
                    "probe.renderer",
                    Severity::Warning,
                    "renderer 与请求不一致（可能回退但未标记）".to_string(),
                    format!("requested={requested} active={active}；fallback 字段未报告为 true。"),
                    evidence.clone(),
                    Confidence::Medium,
                    Some("确认 backend 别名或显式 fallback 字段".to_string()),
                    Some("xtervo diag env --json".to_string()),
                );
                self.add_recommendation(
                    "rec.renderer.explicit",
                    "显式声明 renderer 与 fallback",
                    "在 live diag env 中同时报告 requested/active/fallback，避免用字符串猜测回退。"
                        .to_string(),
                    RecommendationKind::CollectEvidence,
                    evidence,
                    None,
                    finding.as_deref(),
                );
                return;
            }
        }
        if active.is_none() {
            self.add_probe(
                "probe.renderer",
                Layer::Renderer,
                ProbeStatus::Unknown,
                "只报告了请求的 renderer，缺少实际 renderer".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.renderer.active_missing",
                "probe.renderer",
                Severity::Warning,
                "缺少实际 renderer（active）".to_string(),
                format!(
                    "requested={}；active 未收集，不能判断是否回退。",
                    requested.unwrap_or("未收集")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("active 字段出现".to_string()),
                Some("xtervo diag env --json".to_string()),
            );
            return;
        }
        if fallback.is_none() {
            self.add_probe(
                "probe.renderer",
                Layer::Renderer,
                ProbeStatus::Unknown,
                "renderer 已报告，但 fallback 状态缺失".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.renderer.fallback_unknown",
                "probe.renderer",
                Severity::Info,
                "fallback 状态未报告".to_string(),
                format!(
                    "active={}；fallback=null，未当作 false。",
                    active.unwrap_or("未收集")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("fallback 字段出现".to_string()),
                None,
            );
            return;
        }
        let software = active
            .map(|active| active.to_ascii_lowercase().contains("software"))
            .unwrap_or(false);
        let status = if software {
            ProbeStatus::Warn
        } else {
            ProbeStatus::Pass
        };
        self.add_probe(
            "probe.renderer",
            Layer::Renderer,
            status,
            format!(
                "renderer active={} requested={} fallback={}",
                active.unwrap_or("未收集"),
                requested.unwrap_or("未收集"),
                "否"
            ),
            evidence.clone(),
            None,
        );
        if software {
            self.add_finding(
                "finding.renderer.software",
                "probe.renderer",
                Severity::Warning,
                "当前使用软件 renderer".to_string(),
                "active renderer 包含 software；这可能是显式选择，也可能是性能问题的来源。"
                    .to_string(),
                evidence,
                Confidence::Observed,
                Some("active 不再是软件 renderer 且问题消失".to_string()),
                Some("对比 GPU/CPU renderer 的 frame profile".to_string()),
            );
        } else {
            self.add_finding(
                "finding.renderer.ok",
                "probe.renderer",
                Severity::Info,
                "renderer 明确未回退".to_string(),
                format!(
                    "active={} requested={} fallback=false",
                    active.unwrap_or("未收集"),
                    requested.unwrap_or("未收集")
                ),
                evidence,
                Confidence::Observed,
                None,
                None,
            );
        }
    }

    fn check_gpu(&mut self) {
        let env = self.input.environment.clone();
        let evidence = self.refs(self.ids.environment.as_ref());
        let renderer = env.renderer.as_ref();
        let active = renderer.and_then(|renderer| renderer.active.as_deref());
        let requested = renderer.and_then(|renderer| renderer.requested.as_deref());
        let gpu = renderer.and_then(|renderer| renderer.gpu.as_ref());
        let expects_gpu = [active, requested].into_iter().flatten().any(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains("wgpu") || lower.contains("gpu") || lower.contains("vulkan")
        });

        let Some(gpu) = gpu else {
            if expects_gpu {
                self.add_probe(
                    "probe.gpu",
                    Layer::Gpu,
                    ProbeStatus::Unknown,
                    "GPU renderer 未报告 adapter".to_string(),
                    evidence.clone(),
                    None,
                );
                let finding = self.add_finding(
                    "finding.gpu.adapter_missing",
                    "probe.gpu",
                    Severity::Warning,
                    "GPU adapter 未报告".to_string(),
                    "renderer 看起来走 GPU 路径，但 environment.renderer.gpu.adapter 为空；不能判断是否为软件 adapter。".to_string(),
                    evidence.clone(),
                    Confidence::Observed,
                    Some("adapter 字段出现".to_string()),
                    Some("xtervo diag env --json（补充 gpu_adapter）".to_string()),
                );
                self.add_recommendation(
                    "rec.gpu.collect",
                    "采集 GPU adapter/driver",
                    "从 wgpu 初始化或启动日志中补充 adapter、backend、driver；不要把 adapter 名称猜测成 llvmpipe。".to_string(),
                    RecommendationKind::CollectEvidence,
                    evidence,
                    None,
                    finding.as_deref(),
                );
            } else {
                self.add_probe(
                    "probe.gpu",
                    Layer::Gpu,
                    ProbeStatus::Skipped,
                    "renderer 未声明 GPU 路径，跳过 adapter 检查".to_string(),
                    evidence.clone(),
                    None,
                );
                self.add_finding(
                    "finding.gpu.not_applicable",
                    "probe.gpu",
                    Severity::Info,
                    "未声明 GPU 路径".to_string(),
                    "当前 renderer 没有 GPU 标记；缺失 adapter 不构成 GPU 问题。".to_string(),
                    evidence,
                    Confidence::Observed,
                    None,
                    None,
                );
            }
            return;
        };

        let Some(adapter) = &gpu.adapter else {
            self.add_probe(
                "probe.gpu",
                Layer::Gpu,
                ProbeStatus::Unknown,
                "GPU 信息存在但 adapter 缺失".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.gpu.adapter_missing",
                "probe.gpu",
                Severity::Warning,
                "GPU adapter 字段为空".to_string(),
                "未收集 adapter；保留 null，没有填入通用/占位名称。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("adapter 字段出现".to_string()),
                None,
            );
            return;
        };
        let lower = adapter.to_ascii_lowercase();
        let software = [
            "llvmpipe",
            "softpipe",
            "swiftshader",
            "lavapipe",
            "software",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        let status = if software {
            ProbeStatus::Warn
        } else {
            ProbeStatus::Pass
        };
        self.add_probe(
            "probe.gpu",
            Layer::Gpu,
            status,
            format!(
                "GPU adapter={} backend={} driver={}",
                adapter,
                gpu.backend.as_deref().unwrap_or("未收集"),
                gpu.driver.as_deref().unwrap_or("未收集")
            ),
            evidence.clone(),
            None,
        );
        if software {
            self.add_finding(
                "finding.gpu.software_adapter",
                "probe.gpu",
                Severity::Warning,
                "GPU adapter 是软件实现".to_string(),
                format!(
                    "adapter={adapter} backend={}；软件 adapter 的帧时间不能代表真实 GPU。",
                    gpu.backend.as_deref().unwrap_or("未收集")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("换真实 GPU adapter 后同一症状仍存在/消失".to_string()),
                Some("检查驱动与 renderer fallback".to_string()),
            );
            self.add_recommendation(
                "rec.gpu.software",
                "区分软件 adapter 与真实 GPU",
                "记录 adapter/driver，并在真实 GPU 上重复 before/after；不要把 llvmpipe 的慢帧归因于渲染代码。".to_string(),
                RecommendationKind::Probe,
                evidence,
                None,
                Some("finding.gpu.software_adapter"),
            );
        } else {
            self.add_finding(
                "finding.gpu.ok",
                "probe.gpu",
                Severity::Info,
                "GPU adapter 已报告".to_string(),
                format!(
                    "adapter={} backend={} driver={}",
                    adapter,
                    gpu.backend.as_deref().unwrap_or("未收集"),
                    gpu.driver.as_deref().unwrap_or("未收集")
                ),
                evidence,
                Confidence::Observed,
                None,
                None,
            );
        }
    }

    fn check_config(&mut self) {
        let evidence = self.multi_refs(&[
            self.ids.config.as_ref(),
            self.ids.paths.as_ref(),
            self.ids.environment.as_ref(),
        ]);
        let Some(inspection) = self.config_inspection().cloned() else {
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Unknown,
                "config 路径未提供".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.config.path_missing",
                "probe.config",
                Severity::Warning,
                "config 路径未解析".to_string(),
                "没有 config 路径就不能确认配置是否可读/是否含明文凭据；没有假设默认值。"
                    .to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("config 路径出现".to_string()),
                Some("xtervo diag env --json".to_string()),
            );
            self.add_recommendation(
                "rec.config.collect",
                "采集 config 路径",
                "传入 `--config PATH` 或 `xtervo diag env --json`；doctor 只读文件元数据/脱敏扫描，不修改配置。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        };

        if inspection.path.exists == Some(false) {
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Skipped,
                "config 文件不存在，使用默认值".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.config.absent",
                "probe.config",
                Severity::Info,
                "config 文件不存在".to_string(),
                format!("{} 不存在；这不等于配置损坏。", inspection.path.path),
                evidence,
                Confidence::Observed,
                None,
                None,
            );
            return;
        }
        if inspection.path.exists.is_none() {
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Fail,
                "config 路径无法 stat".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.config.stat_failed",
                "probe.config",
                Severity::Error,
                "config 路径不可访问".to_string(),
                format!(
                    "path={} error={}",
                    inspection.path.path,
                    inspection.path.error.as_deref().unwrap_or("未报告")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("路径可 stat".to_string()),
                None,
            );
            self.add_recommendation(
                "rec.config.path",
                "修正 config 路径/权限",
                "确认 `XTERVO_CONFIG` 或默认路径指向可读文件；doctor 不会自动修改配置。".to_string(),
                RecommendationKind::Config,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }
        if inspection.path.is_file != Some(true) || !inspection.readable_text {
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Fail,
                "config 不是可读文件".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.config.unreadable",
                "probe.config",
                Severity::Error,
                "config 不可读".to_string(),
                format!(
                    "path={} is_file={:?} readable={:?} error={}",
                    inspection.path.path,
                    inspection.path.is_file,
                    inspection.path.readable,
                    inspection.read_error.as_deref().unwrap_or("未报告")
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("路径变成可读普通文件".to_string()),
                None,
            );
            self.add_recommendation(
                "rec.config.readable",
                "修复 config 可读性",
                "检查路径、文件类型和权限；不要为了诊断重写用户配置。".to_string(),
                RecommendationKind::Config,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }
        if inspection.utf8 == Some(false) {
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Fail,
                "config 不是 UTF-8 文本".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.config.utf8",
                "probe.config",
                Severity::Error,
                "config 不是 UTF-8".to_string(),
                format!(
                    "path={} scanned={} bytes",
                    inspection.path.path, inspection.scanned_bytes
                ),
                evidence,
                Confidence::Observed,
                Some("文件可被 UTF-8 解码".to_string()),
                None,
            );
            return;
        }

        if inspection.secret_like_matches > 0 {
            let rules = inspection
                .secret_rules
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            self.add_probe(
                "probe.config",
                Layer::Config,
                ProbeStatus::Fail,
                format!(
                    "config 中检测到 {} 处疑似明文凭据（已脱敏）",
                    inspection.secret_like_matches
                ),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.config.secrets",
                "probe.config",
                Severity::Error,
                "config 含疑似明文凭据".to_string(),
                format!(
                    "path={} matches={} rules=[{}] world_accessible={:?}；证据中只保留计数，不保留值。",
                    inspection.path.path,
                    inspection.secret_like_matches,
                    rules,
                    inspection.world_accessible
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("凭据移出配置文件后 matches=0".to_string()),
                Some("用权限/环境变量/askpass 方案替换明文".to_string()),
            );
            self.add_recommendation(
                "rec.config.secrets",
                "移除配置中的明文凭据",
                "把 token/password 放到受控环境变量或 askpass/凭据存储；若必须保留文件，至少收紧到 0600。".to_string(),
                RecommendationKind::Config,
                evidence,
                Some(format!("chmod 600 {}", inspection.path.path)),
                finding.as_deref(),
            );
            return;
        }

        self.add_probe(
            "probe.config",
            Layer::Config,
            ProbeStatus::Pass,
            format!(
                "config 可读，未检测到疑似明文凭据（扫描 {} bytes{}）",
                inspection.scanned_bytes,
                if inspection.scan_truncated {
                    "，已截断"
                } else {
                    ""
                }
            ),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.config.ok",
            "probe.config",
            Severity::Info,
            "config 可读且未发现明文凭据".to_string(),
            format!(
                "path={} bytes={} world_accessible={:?}",
                inspection.path.path, inspection.scanned_bytes, inspection.world_accessible
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn config_inspection(&self) -> Option<&ConfigInspection> {
        self.config_inspection.as_ref()
    }

    fn check_paths(&mut self) {
        let evidence = self.multi_refs(&[self.ids.paths.as_ref(), self.ids.environment.as_ref()]);
        if self.inspections.is_empty() {
            self.add_probe(
                "probe.paths",
                Layer::Files,
                ProbeStatus::Unknown,
                "没有可检查的文件/日志路径".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.paths.none",
                "probe.paths",
                Severity::Warning,
                "未提供文件/日志路径".to_string(),
                "没有路径就无法检查可读性；没有把“不知道”当成可读。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("提供 config/profiles/log/log_dir 路径".to_string()),
                None,
            );
            self.add_recommendation(
                "rec.paths.collect",
                "采集路径",
                "传入 `--config/--profiles/--log/--log-dir` 或 `xtervo diag env --json`。"
                    .to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                None,
                Some("finding.paths.none"),
            );
            return;
        }

        let mut failures = Vec::new();
        let mut missing = Vec::new();
        let mut readable = Vec::new();
        let inspections = self.inspections.clone();
        for inspection in &inspections {
            let expected_kind = if inspection.name == "log_dir" {
                inspection.is_dir == Some(true)
            } else {
                inspection.is_file == Some(true)
            };
            if inspection.exists == Some(false) {
                missing.push(inspection.name.clone());
                continue;
            }
            if inspection.exists.is_none() {
                failures.push(format!(
                    "{}: stat 失败 ({})",
                    inspection.name,
                    inspection.error.as_deref().unwrap_or("未报告")
                ));
                continue;
            }
            if !expected_kind {
                failures.push(format!(
                    "{}: 类型不符（期望 {}）",
                    inspection.name,
                    if inspection.name == "log_dir" {
                        "目录"
                    } else {
                        "普通文件"
                    }
                ));
                continue;
            }
            if inspection.readable != Some(true) {
                failures.push(format!("{}: 不可读", inspection.name));
                continue;
            }
            readable.push(inspection.name.clone());
        }

        if !failures.is_empty() {
            self.add_probe(
                "probe.paths",
                Layer::Files,
                ProbeStatus::Fail,
                format!("文件/日志路径检查失败: {}", failures.join("; ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.paths.unreadable",
                "probe.paths",
                Severity::Error,
                "文件/日志路径不可用".to_string(),
                failures.join("; "),
                evidence.clone(),
                Confidence::Observed,
                Some("路径变为可读且类型正确".to_string()),
                None,
            );
            self.add_recommendation(
                "rec.paths.fix",
                "修复路径/权限",
                "确认路径类型与读权限；`logs` 可能是目录而不是当前日志文件。".to_string(),
                RecommendationKind::Config,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }

        if readable.is_empty() {
            self.add_probe(
                "probe.paths",
                Layer::Files,
                ProbeStatus::Skipped,
                format!("路径尚未生成: {}", missing.join(", ")),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.paths.absent",
                "probe.paths",
                Severity::Info,
                "文件/日志尚未生成".to_string(),
                format!(
                    "不存在: {}；首次运行/未启用日志时属正常，未视为可读。",
                    missing.join(", ")
                ),
                evidence,
                Confidence::Observed,
                None,
                None,
            );
            return;
        }

        self.add_probe(
            "probe.paths",
            Layer::Files,
            ProbeStatus::Pass,
            format!("路径可读: {}", readable.join(", ")),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.paths.ok",
            "probe.paths",
            Severity::Info,
            "文件/日志路径可读".to_string(),
            format!(
                "可读: {}{}",
                readable.join(", "),
                if missing.is_empty() {
                    String::new()
                } else {
                    format!("；尚未生成: {}", missing.join(", "))
                }
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn check_frame(&mut self) {
        let evidence = self.multi_refs(&[self.ids.frame.as_ref(), self.ids.environment.as_ref()]);
        let Some(frame) = self.input.frame.clone() else {
            self.add_probe(
                "probe.frame",
                Layer::Frame,
                ProbeStatus::Unknown,
                "未收集 frame 摘要".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.frame.missing",
                "probe.frame",
                Severity::Info,
                "未收集 frame，不能判断渲染层".to_string(),
                "frame/damage 字段为空；没有填入 0 行/0 damage 冒充健康。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("提供 frame JSON 后重跑".to_string()),
                Some("xtervo diag snapshot --json".to_string()),
            );
            self.add_recommendation(
                "rec.frame.collect",
                "采集 viewport frame",
                "用 `xtervo diag snapshot --json > /tmp/xtervo-frame.json` 抓一次有界视口；不要上传整段 scrollback。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                Some("xtervo diag snapshot --json > /tmp/xtervo-frame.json".to_string()),
                finding.as_deref(),
            );
            return;
        };

        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        if frame.cols == 0 || frame.rows == 0 {
            failures.push(format!("frame 尺寸无效: {}x{}", frame.cols, frame.rows));
        }
        if frame.text_rows_total != frame.rows {
            failures.push(format!(
                "text 行数 {} 与 rows {} 不一致",
                frame.text_rows_total, frame.rows
            ));
        }
        if frame.text.len() < frame.text_rows_total && !frame.text_truncated {
            failures.push("text 行在未标记截断的情况下缺失".to_string());
        }
        if frame.skipped == Some(true) {
            warnings.push("frame.skipped=true（拉帧超时占位）".to_string());
        }
        for image in &frame.images {
            if let (Some(row), Some(col), Some(width), Some(height)) =
                (image.row, image.col, image.width, image.height)
            {
                let outside = row >= frame.rows as i64
                    || row.saturating_add(height as i64) <= 0
                    || col >= frame.cols as u64;
                if outside {
                    warnings.push(format!(
                        "image id={:?} 完全在视口外 (row={row} col={col} {}x{})",
                        image.id, width, height
                    ));
                }
            }
        }

        if !failures.is_empty() {
            self.add_probe(
                "probe.frame",
                Layer::Frame,
                ProbeStatus::Fail,
                format!("frame 摘要契约失败: {}", failures.join("; ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.frame.contract",
                "probe.frame",
                Severity::Error,
                "frame 摘要不一致".to_string(),
                failures.join("; "),
                evidence.clone(),
                Confidence::Observed,
                Some("重新抓帧后行数/尺寸一致".to_string()),
                Some("xtervo diag snapshot --json".to_string()),
            );
            self.add_recommendation(
                "rec.frame.recapture",
                "重新抓取 frame",
                "在问题现场重新抓一次 frame JSON；先确认是采集/序列化问题还是渲染层问题。"
                    .to_string(),
                RecommendationKind::Probe,
                evidence,
                Some("xtervo diag snapshot --json > /tmp/xtervo-frame.json".to_string()),
                finding.as_deref(),
            );
            return;
        }
        if !warnings.is_empty() {
            self.add_probe(
                "probe.frame",
                Layer::Frame,
                ProbeStatus::Warn,
                format!("frame 摘要告警: {}", warnings.join("; ")),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.frame.warn",
                "probe.frame",
                Severity::Warning,
                "frame 层存在可疑信号".to_string(),
                warnings.join("; "),
                evidence,
                Confidence::Observed,
                Some("同一操作下告警消失".to_string()),
                Some("对比前后 frame/scene diff".to_string()),
            );
            return;
        }

        self.add_probe(
            "probe.frame",
            Layer::Frame,
            ProbeStatus::Pass,
            format!(
                "frame 摘要可解析: {}x{} gen={} text_rows={}",
                frame.cols,
                frame.rows,
                frame
                    .generation
                    .map(|generation| generation.to_string())
                    .unwrap_or_else(|| "未收集".to_string()),
                frame.text_rows_total
            ),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.frame.ok",
            "probe.frame",
            Severity::Info,
            "frame 摘要一致".to_string(),
            format!(
                "尺寸、行数、文本截断标记均符合契约；文本摘要 {} 行{}。这只说明采集数据一致，不等于画面正确。",
                frame.text.len(),
                if frame.text_truncated { "（有界裁剪，未读取 scrollback）" } else { "" }
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn check_damage(&mut self) {
        let evidence = self.multi_refs(&[
            self.ids.damage.as_ref(),
            self.ids.frame.as_ref(),
            self.ids.environment.as_ref(),
        ]);
        let Some(frame) = self.input.frame.clone() else {
            self.add_probe(
                "probe.damage",
                Layer::Damage,
                ProbeStatus::Unknown,
                "无 frame，无法检查 damage".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.damage.no_frame",
                "probe.damage",
                Severity::Info,
                "未收集 damage".to_string(),
                "没有 frame 摘要；damage 字段保持未收集。".to_string(),
                evidence,
                Confidence::Observed,
                Some("提供 frame JSON".to_string()),
                None,
            );
            return;
        };
        let Some(damage) = frame.damage.clone() else {
            self.add_probe(
                "probe.damage",
                Layer::Damage,
                ProbeStatus::Unknown,
                "frame JSON 缺少 damage 字段".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.damage.missing",
                "probe.damage",
                Severity::Warning,
                "frame 缺少 damage 摘要".to_string(),
                "无法判断整屏重绘/damage 契约；没有按 0 damage 处理。".to_string(),
                evidence,
                Confidence::Observed,
                Some("damage 字段出现".to_string()),
                Some("xtervo diag snapshot --json".to_string()),
            );
            return;
        };

        let Some(full) = damage.full else {
            self.add_probe(
                "probe.damage",
                Layer::Damage,
                ProbeStatus::Unknown,
                "damage.full 缺失，不能判断整屏重绘".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.damage.full_missing",
                "probe.damage",
                Severity::Info,
                "damage.full 未收集".to_string(),
                "没有把缺失的 full 默认成 false，也不计算整屏覆盖率。".to_string(),
                evidence,
                Confidence::Observed,
                Some("damage.full 出现".to_string()),
                Some("xtervo diag snapshot --json".to_string()),
            );
            return;
        };
        let viewport = (frame.cols as u64).saturating_mul(frame.rows as u64);
        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        if full && damage.reported_regions.unwrap_or(0) > 0 {
            failures.push("full=true 且 region_count>0（契约冲突）".to_string());
        }
        if let Some(area) = damage.area_cells {
            if area > viewport {
                failures.push(format!("damage area {area} > viewport {viewport}"));
            }
        }
        if let Some(coverage) = damage.coverage_ppm {
            if coverage > 1_000_000 {
                failures.push(format!("damage coverage {coverage}ppm > 100%"));
            }
        }
        if damage.rects_truncated {
            warnings.push(format!(
                "damage rects 已裁剪到 {} 个，覆盖率可能不完整",
                damage.rects.len()
            ));
        } else if let Some(reported) = damage.reported_regions {
            if damage.full == Some(false) && reported != damage.rects.len() as u64 {
                warnings.push(format!(
                    "region_count={reported} 与 rects={} 不一致",
                    damage.rects.len()
                ));
            }
        }
        let first_frame = frame.generation.map(|generation| generation <= 1);
        if full {
            match first_frame {
                Some(true) => {}
                Some(false) => warnings.push("full=true（非首帧整屏重绘）".to_string()),
                None => {
                    warnings.push("full=true 且缺少 generation，无法判断是否首帧整屏".to_string())
                }
            }
        } else if !damage.rects_truncated {
            if let Some(coverage) = damage.coverage_ppm {
                if coverage >= self.config.high_damage_ppm {
                    warnings.push(format!(
                        "damage coverage {:.1}% ≥ 阈值 {:.0}%",
                        coverage as f64 / 10_000.0,
                        self.config.high_damage_ppm as f64 / 10_000.0
                    ));
                }
            }
        }

        if !failures.is_empty() {
            self.add_probe(
                "probe.damage",
                Layer::Damage,
                ProbeStatus::Fail,
                format!("damage 契约失败: {}", failures.join("; ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.damage.contract",
                "probe.damage",
                Severity::Error,
                "damage 契约不一致".to_string(),
                failures.join("; "),
                evidence.clone(),
                Confidence::Observed,
                Some("重新抓帧后 area/coverage/region 一致".to_string()),
                Some("xtervo diag scene-diff --left ... --right ...".to_string()),
            );
            self.add_recommendation(
                "rec.damage.diff",
                "用 scene diff 定位 damage 层",
                "抓取前后帧并运行 scene-diff；确认 Full 标记和 rect 是否来自同一帧。".to_string(),
                RecommendationKind::Probe,
                evidence,
                Some(
                    "xtervo diag scene-diff --left /tmp/a.json --right /tmp/b.json --json"
                        .to_string(),
                ),
                finding.as_deref(),
            );
            return;
        }
        if !warnings.is_empty() {
            self.add_probe(
                "probe.damage",
                Layer::Damage,
                ProbeStatus::Warn,
                format!("damage 告警: {}", warnings.join("; ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.damage.warn",
                "probe.damage",
                Severity::Warning,
                "damage 覆盖偏高或契约可疑".to_string(),
                warnings.join("; "),
                evidence.clone(),
                Confidence::Observed,
                Some("非首次帧的 damage 覆盖率下降".to_string()),
                Some("对比连续两帧的 damage".to_string()),
            );
            self.add_recommendation(
                "rec.damage.profile",
                "对比连续帧的 damage",
                "在同一操作下抓两帧，确认 full=true 是否只在首帧/显式 debug 开关出现。".to_string(),
                RecommendationKind::Probe,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }

        self.add_probe(
            "probe.damage",
            Layer::Damage,
            ProbeStatus::Pass,
            format!(
                "damage 摘要正常: full={} regions={} area={} coverage={}ppm",
                full,
                damage
                    .reported_regions
                    .map(|regions| regions.to_string())
                    .unwrap_or_else(|| damage.rects.len().to_string()),
                damage
                    .area_cells
                    .map(|area| area.to_string())
                    .unwrap_or_else(|| "未收集".to_string()),
                damage
                    .coverage_ppm
                    .map(|coverage| coverage.to_string())
                    .unwrap_or_else(|| "未收集".to_string())
            ),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.damage.ok",
            "probe.damage",
            Severity::Info,
            "damage 摘要正常".to_string(),
            format!(
                "full={full}；area/coverage 在视口范围内{}。",
                if full {
                    "（首帧整屏视为正常）"
                } else {
                    ""
                }
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn check_render_profile(&mut self) {
        let evidence = self.multi_refs(&[
            self.ids.render_profile.as_ref(),
            self.ids.environment.as_ref(),
        ]);
        let Some(profile) = self.input.render_profile.clone() else {
            self.add_probe(
                "probe.render_profile",
                Layer::Profile,
                ProbeStatus::Unknown,
                "未收集 render profile".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.profile.missing",
                "probe.render_profile",
                Severity::Info,
                "未收集 render profile".to_string(),
                "没有帧耗时样本；没有填入 0ms 冒充健康。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("提供 profile 后重跑".to_string()),
                Some("XTERVO_RENDER_PROFILE=1".to_string()),
            );
            self.add_recommendation(
                "rec.profile.collect",
                "采集 render profile",
                "用 `XTERVO_RENDER_PROFILE=1` 复现一次并保存日志，再通过 `--render-profile` 传入；只保留有界摘要。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                Some("XTERVO_RENDER_PROFILE=1 xtervo 2>&1 | tee /tmp/xtervo-render.log".to_string()),
                finding.as_deref(),
            );
            return;
        };

        if profile.total_samples() == 0 {
            self.add_probe(
                "probe.render_profile",
                Layer::Profile,
                ProbeStatus::Unknown,
                "render profile 未包含可解析的时序样本".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.profile.no_samples",
                "probe.render_profile",
                Severity::Info,
                "render profile 没有数值样本".to_string(),
                format!(
                    "source={} lines={}；没有解析到 key=number(ms/us/s) 时序，不生成帧时间结论。",
                    profile.source, profile.line_count
                ),
                evidence,
                Confidence::Observed,
                Some("出现可解析的 frame/draw 数值".to_string()),
                None,
            );
            return;
        }

        let slowest = profile.slowest_ms();
        let slow = slowest
            .as_ref()
            .map(|(_, ms)| *ms > self.config.slow_stage_ms)
            .unwrap_or(false);
        if slow {
            let (stage, ms) = slowest.unwrap_or(("未知", 0.0));
            self.add_probe(
                "probe.render_profile",
                Layer::Profile,
                ProbeStatus::Warn,
                format!("render profile 出现慢样本: {stage} max={ms:.2}ms"),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.profile.slow",
                "probe.render_profile",
                Severity::Warning,
                "render profile 出现超阈值阶段".to_string(),
                format!(
                    "stage={stage} max={ms:.2}ms threshold={:.2}ms samples={}",
                    self.config.slow_stage_ms,
                    profile.total_samples()
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("同一操作下该阶段 max 降到阈值内".to_string()),
                Some("抓 frame dump 并对比 CPU raster/upload".to_string()),
            );
            self.add_recommendation(
                "rec.profile.slow",
                "对慢阶段做分层定位",
                "保留 profile 和帧 dump，先区分 CPU raster、upload、present；没有这些数据前不要改 damage/整屏重绘来掩盖。".to_string(),
                RecommendationKind::Probe,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }

        self.add_probe(
            "probe.render_profile",
            Layer::Profile,
            ProbeStatus::Pass,
            format!(
                "render profile 样本 {}，最慢 {:.2}ms（阈值 {:.2}ms）",
                profile.total_samples(),
                slowest.map(|(_, ms)| ms).unwrap_or(0.0),
                self.config.slow_stage_ms
            ),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.profile.ok",
            "probe.render_profile",
            Severity::Info,
            "render profile 未超过阈值".to_string(),
            format!(
                "source={} samples={} lines={}",
                profile.source,
                profile.total_samples(),
                profile.line_count
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn check_metrics(&mut self) {
        let evidence = self.multi_refs(&[self.ids.metrics.as_ref(), self.ids.environment.as_ref()]);
        let Some(metrics) = self.input.metrics.clone() else {
            self.add_probe(
                "probe.metrics",
                Layer::Environment,
                ProbeStatus::Unknown,
                "未收集 metrics".to_string(),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.metrics.missing",
                "probe.metrics",
                Severity::Info,
                "未收集 metrics".to_string(),
                "没有 frame_count/p95；没有把缺数据当成 0ms。".to_string(),
                evidence.clone(),
                Confidence::Observed,
                Some("提供 live metrics 后重跑".to_string()),
                Some("xtervo diag metrics".to_string()),
            );
            self.add_recommendation(
                "rec.metrics.collect",
                "只采集可解释的 metrics",
                "`xtervo diag metrics` 当前是 CLI 占位（initialized=false）；若需要性能判断，先接 live app metrics，否则只用 frame/profile 证据。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                Some("xtervo diag metrics".to_string()),
                finding.as_deref(),
            );
            return;
        };

        if metrics.is_placeholder() {
            self.add_probe(
                "probe.metrics",
                Layer::Environment,
                ProbeStatus::Skipped,
                "metrics 是 CLI 占位（initialized=false/source=cli），不参与健康判断".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.metrics.placeholder",
                "probe.metrics",
                Severity::Info,
                "metrics 是 CLI 占位，零值不代表健康".to_string(),
                format!(
                    "initialized={:?} source={:?}；没有用 0 帧/0 damage 生成 PASS。",
                    metrics.initialized, metrics.source
                ),
                evidence,
                Confidence::Observed,
                Some("initialized=true 且 source 标记为 live".to_string()),
                None,
            );
            return;
        }
        if !metrics.is_live() {
            self.add_probe(
                "probe.metrics",
                Layer::Environment,
                ProbeStatus::Unknown,
                "metrics 未声明实时来源".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.metrics.not_live",
                "probe.metrics",
                Severity::Info,
                "无法确认 metrics 是否实时".to_string(),
                format!(
                    "initialized={:?} source={:?}；没有把未知来源当 live。",
                    metrics.initialized, metrics.source
                ),
                evidence,
                Confidence::Observed,
                Some("initialized=true 且 source=live".to_string()),
                None,
            );
            return;
        }

        let samples = metrics.sample_count.or(metrics.frame_count).unwrap_or(0);
        if samples == 0 {
            self.add_probe(
                "probe.metrics",
                Layer::Environment,
                ProbeStatus::Unknown,
                "live metrics 没有帧样本".to_string(),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.metrics.no_samples",
                "probe.metrics",
                Severity::Info,
                "live metrics 暂无帧样本".to_string(),
                "sample_count/frame_count 为 0 或缺失；没有据此判断帧时间。".to_string(),
                evidence,
                Confidence::Observed,
                Some("出现帧样本".to_string()),
                None,
            );
            return;
        }

        let mut warnings = Vec::new();
        if let Some(p95) = metrics.p95_us {
            if p95 > self.config.slow_frame_p95_us {
                warnings.push(format!(
                    "frame p95={p95}us > {}us",
                    self.config.slow_frame_p95_us
                ));
            }
        } else {
            warnings.push("缺少 frame p95；不能判断帧时间".to_string());
        }
        if let Some(ratio) = metrics.damage_ratio_max_ppm {
            if ratio >= self.config.high_damage_ppm {
                warnings.push(format!("damage ratio max={:.1}%", ratio as f64 / 10_000.0));
            }
        }
        if let Some(stale) = metrics.stale_frames {
            if stale > 0 {
                warnings.push(format!("stale_frames={stale}"));
            }
        }

        let missing_p95 = metrics.p95_us.is_none();
        if !warnings.is_empty() {
            let status = if missing_p95 && metrics.p95_us.is_none() {
                ProbeStatus::Unknown
            } else {
                ProbeStatus::Warn
            };
            self.add_probe(
                "probe.metrics",
                Layer::Environment,
                status,
                format!("metrics 告警: {}", warnings.join("; ")),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.metrics.warn",
                "probe.metrics",
                Severity::Warning,
                "live metrics 超过阈值或缺关键字段".to_string(),
                warnings.join("; "),
                evidence.clone(),
                Confidence::Observed,
                Some("同一场景下 p95/damage 回落到阈值内".to_string()),
                Some("同时抓 render profile 与 frame".to_string()),
            );
            self.add_recommendation(
                "rec.metrics.profile",
                "用 profile 交叉验证 metrics",
                "把 p95/upload/damage 与 render profile、frame 摘要对齐；不要只凭一个计数器下结论。".to_string(),
                RecommendationKind::Probe,
                evidence,
                None,
                finding.as_deref(),
            );
            return;
        }

        self.add_probe(
            "probe.metrics",
            Layer::Environment,
            ProbeStatus::Pass,
            format!(
                "live metrics 在阈值内（samples={samples} p95={}us damage_max={}ppm）",
                metrics.p95_us.unwrap_or(0),
                metrics.damage_ratio_max_ppm.unwrap_or(0)
            ),
            evidence.clone(),
            None,
        );
        self.add_finding(
            "finding.metrics.ok",
            "probe.metrics",
            Severity::Info,
            "live metrics 在阈值内".to_string(),
            format!(
                "samples={samples} p95={:?}us stale={:?}",
                metrics.p95_us, metrics.stale_frames
            ),
            evidence,
            Confidence::Observed,
            None,
            None,
        );
    }

    fn check_evidence(&mut self, stats: EvidenceStats) {
        let evidence = self.refs(self.ids.environment.as_ref());
        if evidence.is_empty() {
            return;
        }
        let truncated = stats.truncated_items > 0 || stats.omissions > 0;
        if truncated {
            self.add_probe(
                "probe.evidence",
                Layer::Evidence,
                ProbeStatus::Warn,
                format!(
                    "证据达到上限: {} 项被截断，{} 项被省略",
                    stats.truncated_items, stats.omissions
                ),
                evidence.clone(),
                None,
            );
            let finding = self.add_finding(
                "finding.evidence.limited",
                "probe.evidence",
                Severity::Warning,
                "证据 bundle 受限，需确认关键证据仍在".to_string(),
                format!(
                    "items={} bytes={} truncated={} omissions={}",
                    stats.item_count, stats.total_bytes, stats.truncated_items, stats.omissions
                ),
                evidence.clone(),
                Confidence::Observed,
                Some("提高上限后不再截断/省略".to_string()),
                None,
            );
            self.add_recommendation(
                "rec.evidence.limit",
                "缩小附件或显式提高证据上限",
                "优先保留环境、frame、metrics、profile 摘要；不要用提高上限来偷偷夹带整段 scrollback/日志。".to_string(),
                RecommendationKind::CollectEvidence,
                evidence,
                None,
                finding.as_deref(),
            );
        } else {
            self.add_probe(
                "probe.evidence",
                Layer::Evidence,
                ProbeStatus::Pass,
                format!(
                    "证据 bundle 正常: {} 项，{} bytes，未截断/省略",
                    stats.item_count, stats.total_bytes
                ),
                evidence.clone(),
                None,
            );
            self.add_finding(
                "finding.evidence.ok",
                "probe.evidence",
                Severity::Info,
                "证据 bundle 完整".to_string(),
                "所有项均通过脱敏与大小上限，bundle 可哈希。".to_string(),
                evidence,
                Confidence::Observed,
                None,
                None,
            );
        }
    }

    fn regression_test(&self) -> RegressionTest {
        let mut candidates: Vec<&Probe> = self
            .probes
            .iter()
            .filter(|probe| probe.status == ProbeStatus::Fail)
            .collect();
        if candidates.is_empty() {
            candidates = self
                .probes
                .iter()
                .filter(|probe| probe.status == ProbeStatus::Warn)
                .collect();
        }
        candidates.sort_by_key(|probe| (probe.layer.order(), probe.id.as_str()));
        let Some(probe) = candidates.first().copied() else {
            return RegressionTest {
                status: RegressionStatus::NotGenerated,
                name: None,
                command: None,
                expected: None,
                evidence: self.refs(self.ids.environment.as_ref()),
                note: "未定位第一处分歧；不生成/伪造回归测试。".to_string(),
            };
        };
        let evidence = if probe.evidence.is_empty() {
            self.refs(self.ids.environment.as_ref())
        } else {
            probe.evidence.clone()
        };
        let terminal_layer = matches!(
            probe.layer,
            Layer::Bytes
                | Layer::Filter
                | Layer::Term
                | Layer::Frame
                | Layer::Damage
                | Layer::Framebuffer
                | Layer::Presenter
                | Layer::Renderer
                | Layer::Gpu
                | Layer::Profile
        );
        if terminal_layer {
            RegressionTest {
                status: RegressionStatus::Proposed,
                name: None,
                command: None,
                expected: Some(format!(
                    "复现同一操作后 `{}` 不再为 {}，且相关 frame/damage/profile 契约保持一致",
                    probe.id,
                    probe.status.as_str()
                )),
                evidence,
                note:
                    "草案：doctor 不自动创建测试；先在对应层补失败测试，再按需加逐字节/逐像素断言。"
                        .to_string(),
            }
        } else {
            RegressionTest {
                status: RegressionStatus::NotApplicable,
                name: None,
                command: None,
                expected: None,
                evidence,
                note: "环境/配置/文件类问题不需要终端回归测试；修复后重跑 doctor 同一命令验证。"
                    .to_string(),
            }
        }
    }
}

fn clean_value(value: &str) -> String {
    truncate_chars(&redact_text(value).0, MAX_PATH_ERROR_CHARS)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::hash::sha256_hex;
    use crate::doctor::ingest::{DamageInfo, RenderProfileSummary};
    use crate::doctor::model::{GpuInfo, RendererInfo};

    fn base_env() -> Environment {
        Environment {
            source: Some("test".to_string()),
            version: Some("0.3.1".to_string()),
            commit: Some("a6afbd1".to_string()),
            dirty: Some(false),
            build_time: Some("2026-11-11T18:00:00Z".to_string()),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            config_path: None,
            profiles_path: None,
            log_path: None,
            log_dir: None,
            ..Environment::default()
        }
    }

    fn run(input: DoctorInput) -> DiagnosticCase {
        Doctor::default().run(&input).expect("doctor run")
    }

    #[test]
    fn every_finding_has_existing_evidence() {
        let input = DoctorInput {
            symptom: Symptom::reported("花屏"),
            environment: Environment {
                renderer: Some(RendererInfo {
                    requested: Some("wgpu".to_string()),
                    active: Some("software".to_string()),
                    fallback: Some(true),
                    fallback_reason: Some("adapter init failed".to_string()),
                    gpu: Some(GpuInfo {
                        adapter: Some("llvmpipe".to_string()),
                        ..GpuInfo::default()
                    }),
                }),
                ..base_env()
            },
            ..DoctorInput::default()
        };
        let case = run(input);
        assert!(!case.findings.is_empty());
        for finding in &case.findings {
            assert!(
                !finding.evidence.is_empty(),
                "finding {} has no evidence",
                finding.id
            );
            for reference in &finding.evidence {
                assert!(case.evidence.items.iter().any(|item| &item.id == reference));
            }
        }
        case.validate().unwrap();
    }

    #[test]
    fn missing_data_is_not_fabricated_and_not_pass() {
        let case = run(DoctorInput {
            environment: Environment::default(),
            ..DoctorInput::default()
        });
        let frame_probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.frame")
            .unwrap();
        assert_eq!(frame_probe.status, ProbeStatus::Unknown);
        let damage_probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.damage")
            .unwrap();
        assert_eq!(damage_probe.status, ProbeStatus::Unknown);
        let metrics_probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.metrics")
            .unwrap();
        assert_eq!(metrics_probe.status, ProbeStatus::Unknown);
        assert!(case.environment.commit.is_none());
        let markdown = &case.issue_draft.markdown;
        assert!(!markdown.contains("0ms"));
        assert!(!markdown.contains("0.0ms"));
        assert!(markdown.contains("未收集"));
        case.validate().unwrap();
    }

    #[test]
    fn placeholder_metrics_are_skipped_not_healthy() {
        let metrics = MetricsSummary {
            initialized: Some(false),
            source: Some("cli".to_string()),
            frame_count: Some(0),
            p95_us: Some(0),
            sample_count: Some(0),
            ..MetricsSummary::default()
        };
        let case = run(DoctorInput {
            environment: base_env(),
            metrics: Some(metrics),
            ..DoctorInput::default()
        });
        let probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.metrics")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::Skipped);
        assert!(case
            .findings
            .iter()
            .any(|finding| finding.id == "finding.metrics.placeholder"));
        assert!(!case
            .findings
            .iter()
            .any(|finding| finding.id == "finding.metrics.ok"));
    }

    #[test]
    fn renderer_fallback_is_fail_with_evidence() {
        let case = run(DoctorInput {
            environment: Environment {
                renderer: Some(RendererInfo {
                    requested: Some("wgpu".to_string()),
                    active: Some("skia".to_string()),
                    fallback: Some(true),
                    fallback_reason: Some("adapter unavailable".to_string()),
                    gpu: None,
                }),
                ..base_env()
            },
            ..DoctorInput::default()
        });
        let probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.renderer")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::Fail);
        let finding = case
            .findings
            .iter()
            .find(|finding| finding.id == "finding.renderer.fallback")
            .unwrap();
        assert_eq!(finding.severity, Severity::Warning);
        assert!(!finding.evidence.is_empty());
        assert!(case.first_divergence().is_some());
    }

    #[test]
    fn frame_damage_contract_is_checked() {
        let frame = FrameSummary {
            schema: Some("xtervo.diag.v1".to_string()),
            kind: Some("snapshot".to_string()),
            cols: 10,
            rows: 2,
            generation: Some(1),
            cursor: None,
            mode: None,
            display_offset: None,
            skipped: None,
            damage: Some(DamageInfo {
                full: Some(true),
                reported_regions: Some(3),
                rects: Vec::new(),
                rects_truncated: false,
                area_cells: Some(20),
                coverage_ppm: Some(1_000_000),
            }),
            images: Vec::new(),
            image_count: 0,
            text: vec!["a".to_string(), "b".to_string()],
            text_rows_total: 2,
            text_truncated: false,
        };
        let case = run(DoctorInput {
            environment: base_env(),
            frame: Some(frame),
            ..DoctorInput::default()
        });
        let probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.damage")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::Fail);
        assert!(case
            .findings
            .iter()
            .any(|finding| finding.id == "finding.damage.contract"));
    }

    #[test]
    fn config_secrets_are_reported_without_leaking_them() {
        let dir = test_dir("config-secrets");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "theme = \"dark\"\napi_key = \"sk-live-1234567890\"\npassword = \"hunter2\"\n",
        )
        .unwrap();
        let case = run(DoctorInput {
            environment: Environment {
                config_path: Some(config.display().to_string()),
                ..base_env()
            },
            ..DoctorInput::default()
        });
        let probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.config")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::Fail);
        let serialized = case.to_json_pretty().unwrap();
        assert!(!serialized.contains("sk-live-1234567890"));
        assert!(!serialized.contains("hunter2"));
        assert!(case
            .findings
            .iter()
            .any(|finding| finding.id == "finding.config.secrets"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn profile_slow_stage_warns() {
        let profile =
            RenderProfileSummary::from_text("test", "frame_ms=50.0 draw_ms=2.0", Some(true), None);
        let case = run(DoctorInput {
            environment: base_env(),
            render_profile: Some(profile),
            ..DoctorInput::default()
        });
        let probe = case
            .probes
            .iter()
            .find(|probe| probe.id == "probe.render_profile")
            .unwrap();
        assert_eq!(probe.status, ProbeStatus::Warn);
        assert!(case
            .findings
            .iter()
            .any(|finding| finding.id == "finding.profile.slow"));
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-doctor-test-{}-{}-{}",
            name,
            std::process::id(),
            &sha256_hex(name.as_bytes())[..8]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
