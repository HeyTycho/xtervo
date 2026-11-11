// SPDX-License-Identifier: GPL-3.0-only
//! Versioned `DiagnosticCase` data model and invariants.
//!
//! A case is deliberately boring data: symptoms, environment, evidence, probes,
//! findings, recommendations, an issue draft and a regression-test plan. It has
//! no timestamps, no random ids and no map iteration order, so the same inputs
//! produce byte-identical JSON. Missing environment/frame/metrics data stays
//! `None`; it is never filled with a plausible-looking zero.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::evidence::{EvidenceBundle, EvidenceVerifyError};
use super::hash::sha256_short;
use super::issue::render_issue;
use super::redact::redact_text;

/// Versioned schema id for the whole case document.
pub const DIAGNOSTIC_CASE_SCHEMA: &str = "tervo.diagnostic_case.v1";
const MAX_USER_TEXT_CHARS: usize = 2_000;
const MAX_REPRO_STEPS: usize = 20;

/// Diagnostic layer. Ordering follows the postmortem ladder: find the first
/// divergence from bytes to the window before blaming config/GPU/build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Bytes,
    Filter,
    Term,
    Frame,
    Damage,
    Framebuffer,
    Presenter,
    Renderer,
    Gpu,
    Profile,
    Config,
    Files,
    Build,
    Environment,
    Evidence,
}

impl Layer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Filter => "filter",
            Self::Term => "term",
            Self::Frame => "frame",
            Self::Damage => "damage",
            Self::Framebuffer => "framebuffer",
            Self::Presenter => "presenter",
            Self::Renderer => "renderer",
            Self::Gpu => "gpu",
            Self::Profile => "profile",
            Self::Config => "config",
            Self::Files => "files",
            Self::Build => "build",
            Self::Environment => "environment",
            Self::Evidence => "evidence",
        }
    }

    /// Lower is earlier in the bytes -> window ladder.
    pub const fn order(self) -> u8 {
        match self {
            Self::Bytes => 0,
            Self::Filter => 1,
            Self::Term => 2,
            Self::Frame => 3,
            Self::Damage => 4,
            Self::Framebuffer => 5,
            Self::Presenter => 6,
            Self::Renderer => 7,
            Self::Gpu => 8,
            Self::Profile => 9,
            Self::Config => 10,
            Self::Files => 11,
            Self::Build => 12,
            Self::Environment => 13,
            Self::Evidence => 14,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    Pass,
    Warn,
    Fail,
    Skipped,
    Unknown,
}

impl ProbeStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }

    pub const fn is_issue(self) -> bool {
        matches!(self, Self::Warn | Self::Fail)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Directly observed in an input document or a path probe.
    Observed,
    High,
    Medium,
    Low,
    /// Only used for a stated hypothesis; evidence is still required.
    Hypothesis,
}

impl Confidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Hypothesis => "hypothesis",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub id: String,
    pub layer: Layer,
    pub status: ProbeStatus,
    pub summary: String,
    pub evidence: Vec<String>,
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    /// Probe that produced this finding. Must exist in the case.
    pub probe: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub evidence: Vec<String>,
    pub confidence: Confidence,
    /// What observation would falsify this finding.
    pub falsify: Option<String>,
    /// Next deterministic probe that would narrow it down.
    pub next_probe: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationKind {
    CollectEvidence,
    Config,
    Probe,
    CodeFix,
    RegressionTest,
}

impl RecommendationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CollectEvidence => "collect_evidence",
            Self::Config => "config",
            Self::Probe => "probe",
            Self::CodeFix => "code_fix",
            Self::RegressionTest => "regression_test",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub id: String,
    pub title: String,
    pub action: String,
    pub kind: RecommendationKind,
    pub evidence: Vec<String>,
    pub command: Option<String>,
    pub related_findings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symptom {
    /// False means "no user symptom was provided"; the check-only case says so
    /// instead of inventing a complaint.
    pub reported: bool,
    pub summary: String,
    pub details: Option<String>,
    pub repro_steps: Vec<String>,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

impl Default for Symptom {
    fn default() -> Self {
        Self::unreported()
    }
}

impl Symptom {
    pub fn unreported() -> Self {
        Self {
            reported: false,
            summary: "未提供症状（仅运行确定性健康检查）".to_string(),
            details: None,
            repro_steps: Vec::new(),
            expected: None,
            actual: None,
        }
    }

    pub fn reported(summary: impl Into<String>) -> Self {
        let summary = cap_chars(summary.into(), MAX_USER_TEXT_CHARS);
        if summary.is_empty() {
            return Self::unreported();
        }
        Self {
            reported: true,
            summary,
            ..Self::unreported()
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        let details = cap_chars(details.into(), MAX_USER_TEXT_CHARS);
        self.details = (!details.is_empty()).then_some(details);
        self
    }

    pub fn with_repro_steps<I, S>(mut self, steps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.repro_steps = steps
            .into_iter()
            .map(|step| cap_chars(step.into(), 500))
            .filter(|step| !step.is_empty())
            .take(MAX_REPRO_STEPS)
            .collect();
        self
    }

    pub fn with_expected_actual(
        mut self,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        let expected = cap_chars(expected.into(), MAX_USER_TEXT_CHARS);
        let actual = cap_chars(actual.into(), MAX_USER_TEXT_CHARS);
        self.expected = (!expected.is_empty()).then_some(expected);
        self.actual = (!actual.is_empty()).then_some(actual);
        self
    }
}

/// Environment as supplied by `tervo diag env --json`, process env or explicit
/// CLI flags. Every field is optional: absent is not the same as default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    pub source: Option<String>,
    pub version: Option<String>,
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub build_time: Option<String>,
    pub target: Option<String>,
    pub os: Option<String>,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub session_type: Option<String>,
    pub desktop: Option<String>,
    pub renderer: Option<RendererInfo>,
    pub config_path: Option<String>,
    pub profiles_path: Option<String>,
    pub log_path: Option<String>,
    pub log_dir: Option<String>,
    pub dri_present: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererInfo {
    /// Renderer requested via config/env (for example `SLINT_BACKEND`).
    pub requested: Option<String>,
    /// Renderer the running app actually reported.
    pub active: Option<String>,
    /// `Some(true)` only when a fallback was explicitly reported.
    pub fallback: Option<bool>,
    pub fallback_reason: Option<String>,
    pub gpu: Option<GpuInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub adapter: Option<String>,
    pub backend: Option<String>,
    pub device_type: Option<String>,
    pub driver: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDraft {
    pub title: String,
    pub markdown: String,
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionStatus {
    /// No divergence was found, so no test should be claimed.
    NotGenerated,
    /// A test plan exists, but it was not created by the doctor.
    Proposed,
    /// The finding is environmental/config-only; a terminal regression test
    /// would not cover it.
    NotApplicable,
}

impl RegressionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotGenerated => "not_generated",
            Self::Proposed => "proposed",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionTest {
    pub status: RegressionStatus,
    /// Never auto-created by the doctor; set only when a real test is known.
    pub name: Option<String>,
    pub command: Option<String>,
    pub expected: Option<String>,
    pub evidence: Vec<String>,
    pub note: String,
}

/// The versioned JSON document the doctor produces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticCase {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub case_id: String,
    pub symptom: Symptom,
    pub environment: Environment,
    pub evidence: EvidenceBundle,
    pub probes: Vec<Probe>,
    pub findings: Vec<Finding>,
    pub recommendations: Vec<Recommendation>,
    pub issue_draft: IssueDraft,
    pub regression_test: RegressionTest,
}

fn default_schema() -> String {
    DIAGNOSTIC_CASE_SCHEMA.to_string()
}

impl DiagnosticCase {
    /// Assemble a case, compute the deterministic id, render the issue draft
    /// and validate all evidence references.
    pub fn assemble(
        symptom: Symptom,
        environment: Environment,
        evidence: EvidenceBundle,
        probes: Vec<Probe>,
        findings: Vec<Finding>,
        recommendations: Vec<Recommendation>,
        regression_test: RegressionTest,
    ) -> Result<Self, ValidationError> {
        let mut case = Self {
            schema: DIAGNOSTIC_CASE_SCHEMA.to_string(),
            case_id: String::new(),
            symptom: sanitize_symptom(symptom),
            environment: sanitize_environment(environment),
            evidence,
            probes,
            findings,
            recommendations,
            issue_draft: IssueDraft::default(),
            regression_test,
        };
        case.case_id = case.compute_case_id();
        case.issue_draft = render_issue(&case);
        case.validate()?;
        Ok(case)
    }

    pub fn first_divergence(&self) -> Option<&Probe> {
        let mut fails: Vec<&Probe> = self
            .probes
            .iter()
            .filter(|probe| probe.status == ProbeStatus::Fail)
            .collect();
        fails.sort_by_key(|probe| (probe.layer.order(), probe.id.as_str()));
        if let Some(probe) = fails.first() {
            return Some(*probe);
        }
        let mut warns: Vec<&Probe> = self
            .probes
            .iter()
            .filter(|probe| probe.status == ProbeStatus::Warn)
            .collect();
        warns.sort_by_key(|probe| (probe.layer.order(), probe.id.as_str()));
        warns.first().copied()
    }

    pub fn severity_counts(&self) -> (usize, usize, usize) {
        let mut info = 0;
        let mut warning = 0;
        let mut error = 0;
        for finding in &self.findings {
            match finding.severity {
                Severity::Info => info += 1,
                Severity::Warning => warning += 1,
                Severity::Error => error += 1,
            }
        }
        (info, warning, error)
    }

    pub fn max_severity(&self) -> Option<Severity> {
        self.findings.iter().map(|finding| finding.severity).max()
    }

    pub fn to_json(&self) -> Result<serde_json::Value, ValidationError> {
        serde_json::to_value(self)
            .map_err(|error| ValidationError(format!("序列化 DiagnosticCase 失败: {error}")))
    }

    pub fn to_json_pretty(&self) -> Result<String, ValidationError> {
        serde_json::to_string_pretty(self)
            .map_err(|error| ValidationError(format!("序列化 DiagnosticCase 失败: {error}")))
    }

    pub fn from_json(text: &str) -> Result<Self, ValidationError> {
        let case: Self = serde_json::from_str(text)
            .map_err(|error| ValidationError(format!("解析 DiagnosticCase 失败: {error}")))?;
        case.validate()?;
        Ok(case)
    }

    fn compute_case_id(&self) -> String {
        let seed = serde_json::json!({
            "symptom": self.symptom,
            "environment": self.environment,
            "evidence_sha256": self.evidence.sha256,
            "probes": self.probes,
            "findings": self.findings,
            "recommendations": self.recommendations,
            "regression_test": self.regression_test,
        });
        let bytes = serde_json::to_vec(&seed).unwrap_or_default();
        format!("case-{}", sha256_short(&bytes, 16))
    }

    /// Enforce the cross-document invariants that make findings auditable.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema != DIAGNOSTIC_CASE_SCHEMA {
            return Err(ValidationError(format!(
                "schema 必须是 {DIAGNOSTIC_CASE_SCHEMA}，实际 {}",
                self.schema
            )));
        }
        if self.case_id.len() != 21
            || !self.case_id.starts_with("case-")
            || !self.case_id[5..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ValidationError(format!(
                "case_id 必须是 case-<16 hex>，实际 {}",
                self.case_id
            )));
        }
        if self.evidence.is_empty() {
            return Err(ValidationError("evidence bundle 不能为空".to_string()));
        }
        self.evidence
            .verify()
            .map_err(|error: EvidenceVerifyError| {
                ValidationError(format!("证据 bundle 校验失败: {error}"))
            })?;

        let mut probe_ids = BTreeSet::new();
        for probe in &self.probes {
            if probe.id.trim().is_empty() {
                return Err(ValidationError("probe.id 不能为空".to_string()));
            }
            if !probe_ids.insert(probe.id.clone()) {
                return Err(ValidationError(format!("重复的 probe.id: {}", probe.id)));
            }
            if probe.summary.trim().is_empty() {
                return Err(ValidationError(format!("probe {} 缺少 summary", probe.id)));
            }
            if probe.evidence.is_empty() {
                return Err(ValidationError(format!("probe {} 没有证据引用", probe.id)));
            }
            self.require_evidence_refs(&probe.evidence, &format!("probe {}", probe.id))?;
        }
        if self.probes.is_empty() {
            return Err(ValidationError("probes 不能为空".to_string()));
        }
        if self.findings.is_empty() {
            return Err(ValidationError("findings 不能为空".to_string()));
        }

        let mut finding_ids = BTreeSet::new();
        for finding in &self.findings {
            if finding.id.trim().is_empty() {
                return Err(ValidationError("finding.id 不能为空".to_string()));
            }
            if !finding_ids.insert(finding.id.clone()) {
                return Err(ValidationError(format!(
                    "重复的 finding.id: {}",
                    finding.id
                )));
            }
            if !probe_ids.contains(&finding.probe) {
                return Err(ValidationError(format!(
                    "finding {} 引用了不存在的 probe {}",
                    finding.id, finding.probe
                )));
            }
            if finding.evidence.is_empty() {
                return Err(ValidationError(format!(
                    "finding {} 没有证据引用；无证据结论不能写成 finding",
                    finding.id
                )));
            }
            if finding.title.trim().is_empty() || finding.detail.trim().is_empty() {
                return Err(ValidationError(format!(
                    "finding {} 缺少 title/detail",
                    finding.id
                )));
            }
            self.require_evidence_refs(&finding.evidence, &format!("finding {}", finding.id))?;
        }

        let mut recommendation_ids = BTreeSet::new();
        for recommendation in &self.recommendations {
            if recommendation.id.trim().is_empty() {
                return Err(ValidationError("recommendation.id 不能为空".to_string()));
            }
            if !recommendation_ids.insert(recommendation.id.clone()) {
                return Err(ValidationError(format!(
                    "重复的 recommendation.id: {}",
                    recommendation.id
                )));
            }
            if recommendation.evidence.is_empty() {
                return Err(ValidationError(format!(
                    "recommendation {} 没有证据引用",
                    recommendation.id
                )));
            }
            self.require_evidence_refs(
                &recommendation.evidence,
                &format!("recommendation {}", recommendation.id),
            )?;
            for finding in &recommendation.related_findings {
                if !finding_ids.contains(finding) {
                    return Err(ValidationError(format!(
                        "recommendation {} 引用了不存在的 finding {}",
                        recommendation.id, finding
                    )));
                }
            }
        }

        self.require_evidence_refs(&self.regression_test.evidence, "regression_test")?;
        if self.issue_draft.title.trim().is_empty() || self.issue_draft.markdown.trim().is_empty() {
            return Err(ValidationError("issue_draft 不能为空".to_string()));
        }
        for heading in [
            "## 现象",
            "## 环境",
            "## 复现",
            "## 证据",
            "## 第一处分歧",
            "## 已排除",
            "## 建议",
            "## 回归测试",
        ] {
            if !self.issue_draft.markdown.contains(heading) {
                return Err(ValidationError(format!(
                    "issue_draft 缺少模板章节 {heading}"
                )));
            }
        }
        Ok(())
    }

    fn require_evidence_refs(&self, refs: &[String], owner: &str) -> Result<(), ValidationError> {
        for reference in refs {
            if !self.evidence.items.iter().any(|item| &item.id == reference) {
                return Err(ValidationError(format!(
                    "{owner} 引用了不存在的证据 {reference}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError(pub String);

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValidationError {}

fn sanitize_symptom(mut symptom: Symptom) -> Symptom {
    symptom.summary = redact_text(&symptom.summary).0;
    symptom.details = symptom.details.map(|text| redact_text(&text).0);
    symptom.repro_steps = symptom
        .repro_steps
        .into_iter()
        .map(|step| redact_text(&step).0)
        .collect();
    symptom.expected = symptom.expected.map(|text| redact_text(&text).0);
    symptom.actual = symptom.actual.map(|text| redact_text(&text).0);
    symptom
}

fn sanitize_environment(mut environment: Environment) -> Environment {
    environment.source = clean_opt(environment.source);
    environment.version = clean_opt(environment.version);
    environment.commit = clean_opt(environment.commit);
    environment.build_time = clean_opt(environment.build_time);
    environment.target = clean_opt(environment.target);
    environment.os = clean_opt(environment.os);
    environment.display = clean_opt(environment.display);
    environment.wayland_display = clean_opt(environment.wayland_display);
    environment.session_type = clean_opt(environment.session_type);
    environment.desktop = clean_opt(environment.desktop);
    environment.config_path = clean_opt(environment.config_path);
    environment.profiles_path = clean_opt(environment.profiles_path);
    environment.log_path = clean_opt(environment.log_path);
    environment.log_dir = clean_opt(environment.log_dir);
    if let Some(renderer) = environment.renderer.as_mut() {
        renderer.requested = clean_opt(renderer.requested.take());
        renderer.active = clean_opt(renderer.active.take());
        renderer.fallback_reason = clean_opt(renderer.fallback_reason.take());
        if let Some(gpu) = renderer.gpu.as_mut() {
            gpu.adapter = clean_opt(gpu.adapter.take());
            gpu.backend = clean_opt(gpu.backend.take());
            gpu.device_type = clean_opt(gpu.device_type.take());
            gpu.driver = clean_opt(gpu.driver.take());
        }
    }
    environment
}

fn clean_opt(value: Option<String>) -> Option<String> {
    value.map(|text| redact_text(&text).0)
}

fn cap_chars(mut text: String, max: usize) -> String {
    text = text.trim().to_string();
    if text.chars().count() <= max {
        return text;
    }
    let mut truncated: String = text.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreported_symptom_is_explicit() {
        let symptom = Symptom::default();
        assert!(!symptom.reported);
        assert!(symptom.summary.contains("未提供"));
        assert!(Symptom::reported("   ").summary.contains("未提供"));
    }

    #[test]
    fn symptom_text_is_bounded() {
        let symptom = Symptom::reported("x".repeat(10_000))
            .with_repro_steps((0..100).map(|index| format!("step {index}")));
        assert!(symptom.summary.chars().count() <= MAX_USER_TEXT_CHARS);
        assert_eq!(symptom.repro_steps.len(), MAX_REPRO_STEPS);
    }

    #[test]
    fn layer_order_matches_postmortem_ladder() {
        assert!(Layer::Bytes.order() < Layer::Filter.order());
        assert!(Layer::Filter.order() < Layer::Term.order());
        assert!(Layer::Term.order() < Layer::Frame.order());
        assert!(Layer::Frame.order() < Layer::Damage.order());
        assert!(Layer::Damage.order() < Layer::Framebuffer.order());
        assert!(Layer::Framebuffer.order() < Layer::Renderer.order());
        assert!(Layer::Config.order() > Layer::Frame.order());
    }
}
