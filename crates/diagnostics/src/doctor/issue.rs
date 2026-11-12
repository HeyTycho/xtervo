// SPDX-License-Identifier: GPL-3.0-only
//! Deterministic issue-draft markdown.
//!
//! The template is fixed (现象/环境/复现/证据/第一处分歧/已排除/建议/回归测试)
//! so maintainers get the same information regardless of whether a model is
//! installed. The draft embeds evidence *metadata* (ids, hashes, sizes), not
//! raw evidence content; the case JSON remains the attachment. Every dynamic
//! value is redacted again as defense in depth.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::model::{DiagnosticCase, IssueDraft, Probe};
use super::redact::redact_text;

const MAX_ISSUE_BYTES: usize = 48 * 1024;
const MAX_EVIDENCE_BULLETS: usize = 32;

/// Render the fixed issue template from a case.
pub fn render_issue(case: &DiagnosticCase) -> IssueDraft {
    let (info_count, warning_count, error_count) = case.severity_counts();
    let issue_count = warning_count + error_count;
    let title = if case.symptom.reported {
        format!("[Doctor] {}", one_line(&case.symptom.summary))
    } else if issue_count == 0 {
        format!("[Doctor] 健康检查：未发现告警（{info_count} 项 info）")
    } else {
        format!("[Doctor] 健康检查：{issue_count} 项告警/错误")
    };
    let title = truncate_chars(&title, 120);

    let mut body = String::with_capacity(8 * 1024);
    writeln!(body, "# {title}\n").expect("String write cannot fail");
    writeln!(
        body,
        "> 由 `tervo-doctor` 确定性生成；未调用模型或网络。schema `{}`，case `{}`。\n",
        case.schema, case.case_id
    )
    .expect("String write cannot fail");

    render_symptom(&mut body, case);
    render_environment(&mut body, case);
    render_repro(&mut body, case);
    render_evidence(&mut body, case);
    render_divergence(&mut body, case);
    render_excluded(&mut body, case);
    render_recommendations(&mut body, case);
    render_regression(&mut body, case);

    let markdown = cap_markdown(body);
    let mut labels: BTreeSet<String> = ["doctor", "diagnostics"]
        .iter()
        .map(|label| (*label).to_string())
        .collect();
    if let Some(severity) = case.max_severity() {
        labels.insert(format!("severity:{}", severity.as_str()));
    }
    let probes: std::collections::BTreeMap<&str, &Probe> = case
        .probes
        .iter()
        .map(|probe| (probe.id.as_str(), probe))
        .collect();
    for finding in &case.findings {
        if let Some(probe) = probes.get(finding.probe.as_str()) {
            labels.insert(format!("layer:{}", probe.layer.as_str()));
        }
    }
    IssueDraft {
        title,
        markdown,
        labels: labels.into_iter().collect(),
    }
}

fn render_symptom(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 现象\n");
    body.push_str(&quote_block(&clean(&case.symptom.summary)));
    if let Some(details) = &case.symptom.details {
        body.push('\n');
        body.push_str(&quote_block(&clean(details)));
    }
    if let Some(expected) = &case.symptom.expected {
        writeln!(body, "- 期望：{}", one_line(expected)).expect("String write cannot fail");
    }
    if let Some(actual) = &case.symptom.actual {
        writeln!(body, "- 实际：{}", one_line(actual)).expect("String write cannot fail");
    }
    if !case.symptom.reported {
        body.push_str("- 说明：调用方未提供用户症状，本 case 只覆盖确定性健康检查。\n");
    }
    body.push('\n');
}

fn render_environment(body: &mut String, case: &DiagnosticCase) {
    let env = &case.environment;
    body.push_str("## 环境\n");
    env_line(body, "版本", env.version.as_deref());
    env_line(body, "commit", env.commit.as_deref());
    env_line(
        body,
        "工作树",
        match env.dirty {
            Some(true) => Some("dirty（commit 不能唯一标识代码）"),
            Some(false) => Some("clean"),
            None => None,
        },
    );
    env_line(body, "构建时间", env.build_time.as_deref());
    env_line(body, "目标平台", env.target.as_deref());
    env_line(body, "OS", env.os.as_deref());
    env_line(body, "会话", env.session_type.as_deref());
    env_line(body, "桌面", env.desktop.as_deref());
    env_line(body, "DISPLAY", env.display.as_deref());
    env_line(body, "WAYLAND_DISPLAY", env.wayland_display.as_deref());
    env_line(
        body,
        "DRI",
        env.dri_present
            .map(|present| if present { "存在" } else { "不存在" }),
    );
    if let Some(renderer) = &env.renderer {
        env_line(body, "renderer 请求", renderer.requested.as_deref());
        env_line(body, "renderer 实际", renderer.active.as_deref());
        env_line(
            body,
            "renderer fallback",
            renderer
                .fallback
                .map(|fallback| if fallback { "是" } else { "否" }),
        );
        env_line(body, "fallback 原因", renderer.fallback_reason.as_deref());
        if let Some(gpu) = &renderer.gpu {
            env_line(body, "GPU adapter", gpu.adapter.as_deref());
            env_line(body, "GPU backend", gpu.backend.as_deref());
            env_line(body, "GPU 类型", gpu.device_type.as_deref());
            env_line(body, "GPU 驱动", gpu.driver.as_deref());
        }
    }
    env_line(body, "配置", env.config_path.as_deref());
    env_line(body, "profiles", env.profiles_path.as_deref());
    env_line(body, "日志文件", env.log_path.as_deref());
    env_line(body, "日志目录", env.log_dir.as_deref());
    env_line(body, "环境来源", env.source.as_deref());
    body.push('\n');
}

fn env_line(body: &mut String, label: &str, value: Option<&str>) {
    let value = value.map(clean).filter(|value| !value.trim().is_empty());
    match value {
        Some(value) => {
            writeln!(body, "- {label}: {}", one_line(&value)).expect("String write cannot fail")
        }
        None => writeln!(body, "- {label}: 未收集").expect("String write cannot fail"),
    }
}

fn render_repro(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 复现\n");
    if case.symptom.repro_steps.is_empty() {
        body.push_str("- 未提供复现步骤。\n");
        body.push_str("- 若问题可复现，请附上 `tervo diag env --json` 与 `tervo diag snapshot --json` 的输出（先脱敏）。\n");
    } else {
        for (index, step) in case.symptom.repro_steps.iter().enumerate() {
            writeln!(body, "{}. {}", index + 1, one_line(step)).expect("String write cannot fail");
        }
    }
    body.push('\n');
}

fn render_evidence(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 证据\n");
    writeln!(
        body,
        "- case：`{}`，schema：`{}`",
        case.case_id, case.schema
    )
    .expect("String write cannot fail");
    writeln!(
        body,
        "- bundle：`{}`（{} 项，{} bytes，限额 {} KiB/项，{} KiB 总计）",
        case.evidence.sha256,
        case.evidence.items.len(),
        case.evidence.total_bytes,
        case.evidence.limits.max_item_bytes / 1024,
        case.evidence.limits.max_total_bytes / 1024
    )
    .expect("String write cannot fail");
    body.push_str("- 采集边界：只包含有界视口/日志摘要与显式附件；不读取整段 scrollback；证据内容已经脱敏并逐项哈希。\n");
    for item in case.evidence.items.iter().take(MAX_EVIDENCE_BULLETS) {
        let mut flags = Vec::new();
        if item.truncated {
            flags.push("已截断");
        }
        if item.redaction.matches > 0 {
            flags.push("有脱敏");
        }
        let flag_text = if flags.is_empty() {
            String::new()
        } else {
            format!("；{}", flags.join("，"))
        };
        writeln!(
            body,
            "- `[{}]` {}（{}，{} bytes，sha256 `{}`{}）",
            item.id,
            one_line(&item.label),
            item.kind.as_str(),
            item.stored_bytes,
            short_hash(&item.sha256),
            flag_text
        )
        .expect("String write cannot fail");
    }
    if case.evidence.items.len() > MAX_EVIDENCE_BULLETS {
        writeln!(
            body,
            "- 其余 {} 项证据见 case JSON。",
            case.evidence.items.len() - MAX_EVIDENCE_BULLETS
        )
        .expect("String write cannot fail");
    }
    if !case.evidence.omissions.is_empty() {
        writeln!(
            body,
            "- 注意：有 {} 项证据因大小/数量上限被省略或截断。",
            case.evidence.omissions.len()
        )
        .expect("String write cannot fail");
    }
    body.push('\n');
}

fn render_divergence(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 第一处分歧\n");
    match case.first_divergence() {
        Some(probe) => {
            let kind = if probe.status == super::model::ProbeStatus::Fail {
                "hard fail"
            } else {
                "最早告警（未达到 hard fail）"
            };
            writeln!(
                body,
                "- `{}`（{}，{}，{}）：{}",
                probe.id,
                probe.layer.as_str(),
                probe.status.as_str(),
                kind,
                one_line(&probe.summary)
            )
            .expect("String write cannot fail");
            if !probe.evidence.is_empty() {
                writeln!(body, "- 证据引用：`{}`", probe.evidence.join("`, `"))
                    .expect("String write cannot fail");
            }
            if let Some(command) = &probe.command {
                writeln!(body, "- 复现命令：`{}`", one_line(command))
                    .expect("String write cannot fail");
            }
            for finding in case
                .findings
                .iter()
                .filter(|finding| finding.probe == probe.id)
                .take(4)
            {
                writeln!(
                    body,
                    "- {}（{}，confidence={}）：{}",
                    finding.title,
                    finding.severity.as_str(),
                    finding.confidence.as_str(),
                    one_line(&finding.detail)
                )
                .expect("String write cannot fail");
            }
        }
        None => {
            body.push_str("- 未定位：本 case 未执行 bytes → filter → Term → Frame → framebuffer 分层探针；缺失数据没有被当作 PASS。\n");
        }
    }
    body.push('\n');
}

fn render_excluded(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 已排除\n");
    let mut excluded = 0usize;
    let mut unchecked = Vec::new();
    for probe in &case.probes {
        match probe.status {
            super::model::ProbeStatus::Pass => {
                writeln!(
                    body,
                    "- `{}`（{}）：{}",
                    probe.id,
                    probe.layer.as_str(),
                    one_line(&probe.summary)
                )
                .expect("String write cannot fail");
                excluded += 1;
            }
            super::model::ProbeStatus::Skipped | super::model::ProbeStatus::Unknown => {
                unchecked.push(format!(
                    "`{}`（{}）：{}",
                    probe.id,
                    probe.layer.as_str(),
                    one_line(&probe.summary)
                ));
            }
            super::model::ProbeStatus::Warn | super::model::ProbeStatus::Fail => {}
        }
    }
    if excluded == 0 {
        body.push_str("- 暂无可排除项（缺数据/跳过不会记为 PASS）。\n");
    }
    if !unchecked.is_empty() {
        body.push_str("- 未验证：\n");
        for line in unchecked {
            writeln!(body, "  - {line}").expect("String write cannot fail");
        }
    }
    body.push('\n');
}

fn render_recommendations(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 建议\n");
    if case.recommendations.is_empty() {
        body.push_str("- 暂无（没有证据时不做建议）。\n");
    } else {
        for recommendation in &case.recommendations {
            writeln!(
                body,
                "- **{}**（{}）：{}",
                one_line(&recommendation.title),
                recommendation.kind.as_str(),
                one_line(&recommendation.action)
            )
            .expect("String write cannot fail");
            if let Some(command) = &recommendation.command {
                writeln!(body, "  - 命令：`{}`", one_line(command))
                    .expect("String write cannot fail");
            }
            if !recommendation.evidence.is_empty() {
                writeln!(body, "  - 证据：`{}`", recommendation.evidence.join("`, `"))
                    .expect("String write cannot fail");
            }
        }
    }
    body.push('\n');
}

fn render_regression(body: &mut String, case: &DiagnosticCase) {
    body.push_str("## 回归测试\n");
    let regression = &case.regression_test;
    writeln!(body, "- 状态：{}", regression.status.as_str()).expect("String write cannot fail");
    if let Some(name) = &regression.name {
        writeln!(body, "- 测试：`{}`", one_line(name)).expect("String write cannot fail");
    }
    if let Some(command) = &regression.command {
        writeln!(body, "- 命令：`{}`", one_line(command)).expect("String write cannot fail");
    }
    if let Some(expected) = &regression.expected {
        writeln!(body, "- 断言：{}", one_line(expected)).expect("String write cannot fail");
    }
    writeln!(body, "- 说明：{}", one_line(&regression.note)).expect("String write cannot fail");
    if !regression.evidence.is_empty() {
        writeln!(body, "- 证据：`{}`", regression.evidence.join("`, `"))
            .expect("String write cannot fail");
    }
    body.push('\n');
}

fn quote_block(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines().take(24) {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    if text.lines().count() > 24 {
        out.push_str("> …\n");
    }
    out
}

fn clean(text: &str) -> String {
    redact_text(text).0
}

fn one_line(text: &str) -> String {
    let cleaned = clean(text);
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&joined, 600)
}

fn short_hash(hash: &str) -> &str {
    hash.get(..16).unwrap_or(hash)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn cap_markdown(mut markdown: String) -> String {
    if markdown.len() <= MAX_ISSUE_BYTES {
        return markdown;
    }
    let mut end = MAX_ISSUE_BYTES;
    while end > 0 && !markdown.is_char_boundary(end) {
        end -= 1;
    }
    markdown.truncate(end);
    markdown.push_str("\n\n… issue draft 已按大小上限截断，完整内容见 case JSON。\n");
    markdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::evidence::{EvidenceBundleBuilder, EvidenceKind, EvidenceLimits};
    use crate::doctor::model::Layer;
    use crate::doctor::model::{
        Confidence, Environment, Finding, Probe, ProbeStatus, Recommendation, RecommendationKind,
        RegressionStatus, RegressionTest, Severity, Symptom,
    };

    fn sample_case(symptom: Symptom) -> DiagnosticCase {
        let mut builder = EvidenceBundleBuilder::new(EvidenceLimits::default());
        let evidence = builder
            .add_text(EvidenceKind::Environment, "environment", "commit=abc123")
            .expect("evidence");
        let probe = Probe {
            id: "probe.frame".to_string(),
            layer: Layer::Frame,
            status: ProbeStatus::Warn,
            summary: "frame damage 覆盖整屏".to_string(),
            evidence: vec![evidence.clone()],
            command: None,
        };
        let finding = Finding {
            id: "finding.frame.full".to_string(),
            probe: probe.id.clone(),
            severity: Severity::Warning,
            title: "整屏 damage".to_string(),
            detail: "frame 报告 full=true".to_string(),
            evidence: vec![evidence.clone()],
            confidence: Confidence::Observed,
            falsify: Some("重新抓帧后 full=false".to_string()),
            next_probe: Some("capture incremental frame".to_string()),
        };
        let recommendation = Recommendation {
            id: "rec.frame.profile".to_string(),
            title: "采集 render profile".to_string(),
            action: "用 TERVO_RENDER_PROFILE=1 复现一次".to_string(),
            kind: RecommendationKind::Probe,
            evidence: vec![evidence.clone()],
            command: Some("TERVO_RENDER_PROFILE=1 tervo".to_string()),
            related_findings: vec![finding.id.clone()],
        };
        let regression = RegressionTest {
            status: RegressionStatus::Proposed,
            name: None,
            command: None,
            expected: Some("damage 不再覆盖整屏".to_string()),
            evidence: vec![evidence.clone()],
            note: "草案，不自动创建测试".to_string(),
        };
        let bundle = builder.finish();
        DiagnosticCase::assemble(
            symptom,
            Environment {
                version: Some("0.3.1".to_string()),
                commit: Some("abc123".to_string()),
                ..Environment::default()
            },
            bundle,
            vec![probe],
            vec![finding],
            vec![recommendation],
            regression,
        )
        .expect("case should assemble")
    }

    fn heading_positions(markdown: &str) -> Vec<usize> {
        [
            "## 现象",
            "## 环境",
            "## 复现",
            "## 证据",
            "## 第一处分歧",
            "## 已排除",
            "## 建议",
            "## 回归测试",
        ]
        .iter()
        .map(|heading| {
            markdown
                .find(heading)
                .unwrap_or_else(|| panic!("missing {heading}"))
        })
        .collect()
    }

    #[test]
    fn template_has_required_sections_in_order() {
        let case = sample_case(Symptom::reported("滚动后残影"));
        let positions = heading_positions(&case.issue_draft.markdown);
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(case.issue_draft.markdown.contains("## 第一处分歧"));
        assert!(case.issue_draft.markdown.contains("sha256"));
    }

    #[test]
    fn issue_redacts_user_supplied_secrets() {
        let case = sample_case(
            Symptom::reported("token=ghp_abcdefghijklmnopqrstuvwxyz0123456789 后花屏")
                .with_details("password=hunter2"),
        );
        let markdown = &case.issue_draft.markdown;
        assert!(!markdown.contains("ghp_"));
        assert!(!markdown.contains("hunter2"));
        assert!(markdown.contains("[REDACTED]"));
    }

    #[test]
    fn title_falls_back_for_check_only_cases() {
        let case = sample_case(Symptom::default());
        assert!(case.issue_draft.title.starts_with("[Doctor] 健康检查："));
        assert!(case
            .issue_draft
            .labels
            .iter()
            .any(|label| label == "layer:frame"));
    }
}
