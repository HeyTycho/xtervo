// SPDX-License-Identifier: GPL-3.0-only
//! Thin CLI over `tervo_diagnostics::doctor`.
//!
//! No window, no Slint, no PTY, no network, no AI provider. It reads only the
//! files explicitly passed plus documented environment variables, then prints
//! a versioned `tervo.diagnostic_case.v1` JSON document or its issue draft.
//!
//! The same library API is what `tervo diag` should call; this binary exists so
//! the slice can be exercised end-to-end before app wiring.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;
use tervo_diagnostics::doctor::{
    diagnostic_case_json_schema, Attachment, DiagnosticCase, Doctor, DoctorConfig, DoctorInput,
    Environment, EvidenceKind, FrameSummary, MetricsSummary, RenderProfileSummary, Symptom,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = match option_env!("TERVO_GIT_COMMIT") {
    Some(value) => value,
    None => "unknown",
};
const DIRTY: &str = match option_env!("TERVO_GIT_DIRTY") {
    Some(value) => value,
    None => "0",
};
const BUILT: &str = match option_env!("TERVO_BUILD_TIME") {
    Some(value) => value,
    None => "unknown",
};
const TARGET: &str = match option_env!("TERVO_BUILD_TARGET") {
    Some(value) => value,
    None => "unknown",
};

const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ATTACH_BYTES: u64 = 64 * 1024;
const MAX_PROFILE_BYTES: usize = 128 * 1024;

#[derive(Debug)]
struct CliError(String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

fn err<T>(message: impl Into<String>) -> Result<T, Box<dyn std::error::Error>> {
    Err(Box::new(CliError(message.into())))
}

#[derive(Debug, Default)]
struct Args {
    command: String,
    values: BTreeMap<String, Vec<String>>,
    flags: BTreeSet<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .and_then(|values| values.last())
            .map(String::as_str)
    }

    fn values(&self, name: &str) -> &[String] {
        self.values.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("[tervo-doctor] {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8, Box<dyn std::error::Error>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&raw)?;
    match args.command.as_str() {
        "help" => {
            print_help();
            Ok(0)
        }
        "version" => {
            print_version();
            Ok(0)
        }
        "schema" => command_schema(&args),
        "env" => command_env(&args),
        "issue" => command_issue(&args),
        "check" | "doctor" => command_check(&args),
        other => err(format!("未知子命令: {other}（用 `tervo-doctor help`）")),
    }
}

fn parse_args(raw: &[String]) -> Result<Args, Box<dyn std::error::Error>> {
    let mut parsed = Args {
        command: "check".to_string(),
        ..Args::default()
    };
    let mut start = 0usize;
    if let Some(first) = raw.first() {
        if !first.starts_with('-') {
            parsed.command = first.clone();
            start = 1;
        }
    }
    let mut index = start;
    while index < raw.len() {
        let arg = &raw[index];
        if arg == "--" {
            break;
        }
        if arg == "-h" {
            parsed.flags.insert("help".to_string());
            index += 1;
            continue;
        }
        if arg == "--version" {
            parsed.flags.insert("version".to_string());
            index += 1;
            continue;
        }
        if arg == "--help" {
            parsed.flags.insert("help".to_string());
            index += 1;
            continue;
        }
        let Some(body) = arg.strip_prefix("--") else {
            return err(format!("未知位置参数: {arg}"));
        };
        let (name, inline_value) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (body, None),
        };
        let name = name.to_string();
        if is_boolean_flag(&name) {
            if inline_value.is_some() {
                return err(format!("--{name} 不接受值"));
            }
            parsed.flags.insert(name);
            index += 1;
            continue;
        }
        if !is_value_flag(&name) {
            return err(format!("未知选项: --{name}"));
        }
        let value = match inline_value {
            Some(value) => value,
            None => {
                index += 1;
                let Some(value) = raw.get(index) else {
                    return err(format!("--{name} 缺少数值/路径"));
                };
                value.clone()
            }
        };
        parsed.values.entry(name).or_default().push(value);
        index += 1;
    }
    if parsed.flag("help") || parsed.flag("version") {
        parsed.command = if parsed.flag("version") {
            "version"
        } else {
            "help"
        }
        .to_string();
    }
    Ok(parsed)
}

fn is_boolean_flag(name: &str) -> bool {
    matches!(
        name,
        "compact" | "no-fallback" | "help" | "version" | "json" | "quiet"
    )
}

fn is_value_flag(name: &str) -> bool {
    matches!(
        name,
        "symptom"
            | "details"
            | "repro"
            | "expected"
            | "actual"
            | "diag-env"
            | "frame"
            | "metrics"
            | "render-profile"
            | "config"
            | "profiles"
            | "log"
            | "log-dir"
            | "active-renderer"
            | "requested-renderer"
            | "fallback-reason"
            | "gpu-adapter"
            | "gpu-backend"
            | "gpu-driver"
            | "gpu-device-type"
            | "attach"
            | "attach-tail"
            | "case"
            | "out"
            | "fail-on"
            | "max-evidence-bytes"
            | "max-total-bytes"
            | "max-evidence-items"
    )
}

fn command_check(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let input = build_input(args)?;
    let doctor = Doctor::new(build_config(args)?);
    let case = doctor.run(&input)?;
    emit_case(args, &case)?;
    exit_code(&case, args.value("fail-on"))
}

fn command_issue(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let case = if let Some(path) = args.value("case") {
        let text = read_text_path(path, MAX_JSON_BYTES)?;
        DiagnosticCase::from_json(&text)?
    } else {
        let input = build_input(args)?;
        Doctor::new(build_config(args)?).run(&input)?
    };
    if args.flag("json") {
        emit_case(args, &case)?;
    } else {
        emit(args, &case.issue_draft.markdown)?;
    }
    exit_code(&case, args.value("fail-on"))
}

fn command_schema(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let schema = diagnostic_case_json_schema();
    let text = if args.flag("compact") {
        serde_json::to_string(&schema)?
    } else {
        serde_json::to_string_pretty(&schema)?
    };
    emit(args, &text)?;
    Ok(0)
}

fn command_env(args: &Args) -> Result<u8, Box<dyn std::error::Error>> {
    let env = build_environment(args)?;
    let text = if args.flag("compact") {
        serde_json::to_string(&env)?
    } else {
        serde_json::to_string_pretty(&env)?
    };
    emit(args, &text)?;
    Ok(0)
}

fn build_config(args: &Args) -> Result<DoctorConfig, Box<dyn std::error::Error>> {
    let mut config = DoctorConfig::default();
    if let Some(value) = args.value("max-evidence-bytes") {
        config.evidence_limits.max_item_bytes =
            parse_usize("max-evidence-bytes", value, 1, 8 * 1024 * 1024)?;
    }
    if let Some(value) = args.value("max-total-bytes") {
        config.evidence_limits.max_total_bytes =
            parse_usize("max-total-bytes", value, 1, 32 * 1024 * 1024)?;
    }
    if let Some(value) = args.value("max-evidence-items") {
        config.evidence_limits.max_items = parse_usize("max-evidence-items", value, 1, 4096)?;
    }
    Ok(config)
}

fn build_input(args: &Args) -> Result<DoctorInput, Box<dyn std::error::Error>> {
    let environment = build_environment(args)?;
    let frame = match args.value("frame") {
        Some(path) => {
            let text = read_text_path(path, MAX_JSON_BYTES)?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| CliError(format!("--frame 不是 JSON: {error}")))?;
            Some(
                FrameSummary::from_diag_json_with_limits(&value, 40, 200)
                    .map_err(|error| CliError(format!("解析 frame 失败: {error}")))?,
            )
        }
        None => None,
    };
    let metrics = match args.value("metrics") {
        Some(path) => {
            let text = read_text_path(path, MAX_JSON_BYTES)?;
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| CliError(format!("--metrics 不是 JSON: {error}")))?;
            Some(
                MetricsSummary::from_diag_json(&value)
                    .map_err(|error| CliError(format!("解析 metrics 失败: {error}")))?,
            )
        }
        None => None,
    };
    let render_profile = match args.value("render-profile") {
        Some(path) => {
            let text = read_text_path(path, MAX_PROFILE_BYTES as u64)?;
            let enabled = std::env::var_os("TERVO_RENDER_PROFILE").is_some();
            Some(RenderProfileSummary::from_text(
                path,
                &text,
                Some(enabled),
                Some(MAX_PROFILE_BYTES),
            ))
        }
        None => None,
    };
    let symptom = build_symptom(args);
    let mut attachments = Vec::new();
    for spec in args.values("attach") {
        let (label, path) = split_attachment(spec)?;
        let content = read_prefix(&path, MAX_ATTACH_BYTES)?;
        attachments.push(Attachment {
            label,
            content,
            kind: EvidenceKind::Attachment,
        });
    }
    for spec in args.values("attach-tail") {
        let (label, path) = split_attachment(spec)?;
        let content = read_tail(&path, MAX_ATTACH_BYTES)?;
        attachments.push(Attachment {
            label,
            content,
            kind: EvidenceKind::Logs,
        });
    }
    Ok(DoctorInput {
        symptom,
        environment,
        frame,
        metrics,
        render_profile,
        attachments,
    })
}

fn build_symptom(args: &Args) -> Symptom {
    let Some(summary) = args.value("symptom") else {
        return Symptom::default();
    };
    let mut symptom = Symptom::reported(summary);
    if let Some(details) = args.value("details") {
        symptom = symptom.with_details(details);
    }
    let steps: Vec<&str> = args.values("repro").iter().map(String::as_str).collect();
    if !steps.is_empty() {
        symptom = symptom.with_repro_steps(steps);
    }
    if let (Some(expected), Some(actual)) = (args.value("expected"), args.value("actual")) {
        symptom = symptom.with_expected_actual(expected, actual);
    }
    symptom
}

fn build_environment(args: &Args) -> Result<Environment, Box<dyn std::error::Error>> {
    let mut environment = if let Some(path) = args.value("diag-env") {
        let text = read_text_path(path, MAX_JSON_BYTES)?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| CliError(format!("--diag-env 不是 JSON: {error}")))?;
        Environment::from_diag_env_json(&value)
            .map_err(|error| CliError(format!("解析 diag env 失败: {error}")))?
    } else {
        Environment::from_process()
    };
    if let Some(value) = args.value("config") {
        environment.config_path = Some(value.to_string());
    }
    if let Some(value) = args.value("profiles") {
        environment.profiles_path = Some(value.to_string());
    }
    if let Some(value) = args.value("log") {
        environment.log_path = Some(value.to_string());
    }
    if let Some(value) = args.value("log-dir") {
        environment.log_dir = Some(value.to_string());
    }

    let mut renderer = environment.renderer.take().unwrap_or_default();
    let mut renderer_touched = false;
    if let Some(value) = args.value("active-renderer") {
        renderer.active = Some(value.to_string());
        renderer_touched = true;
    }
    if let Some(value) = args.value("requested-renderer") {
        renderer.requested = Some(value.to_string());
        renderer_touched = true;
    }
    if let Some(value) = args.value("fallback-reason") {
        renderer.fallback = Some(true);
        renderer.fallback_reason = Some(value.to_string());
        renderer_touched = true;
    }
    if args.flag("no-fallback") {
        renderer.fallback = Some(false);
        renderer_touched = true;
    }
    let mut gpu = renderer.gpu.take().unwrap_or_default();
    let mut gpu_touched = false;
    if let Some(value) = args.value("gpu-adapter") {
        gpu.adapter = Some(value.to_string());
        gpu_touched = true;
    }
    if let Some(value) = args.value("gpu-backend") {
        gpu.backend = Some(value.to_string());
        gpu_touched = true;
    }
    if let Some(value) = args.value("gpu-driver") {
        gpu.driver = Some(value.to_string());
        gpu_touched = true;
    }
    if let Some(value) = args.value("gpu-device-type") {
        gpu.device_type = Some(value.to_string());
        gpu_touched = true;
    }
    if gpu_touched {
        renderer.gpu = Some(gpu);
        renderer_touched = true;
    }
    if renderer_touched {
        environment.renderer = Some(renderer);
    }
    Ok(environment)
}

fn split_attachment(spec: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let (label, path) = match spec.split_once('=') {
        Some((label, path)) if !label.is_empty() => (label.to_string(), path.to_string()),
        Some(_) => return err("--attach 的 label 不能为空"),
        None => {
            let path = PathBuf::from(spec);
            let label = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("attachment")
                .to_string();
            (label, spec.to_string())
        }
    };
    Ok((label, path))
}

fn read_text_path(path: &str, max_bytes: u64) -> Result<String, Box<dyn std::error::Error>> {
    if path == "-" {
        return read_stdin_capped(max_bytes);
    }
    let bytes = read_prefix_bytes(path, max_bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_prefix(path: &str, max_bytes: u64) -> Result<String, Box<dyn std::error::Error>> {
    let bytes = read_prefix_bytes(path, max_bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_prefix_bytes(path: &str, max_bytes: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if path == "-" {
        return read_stdin_bytes(max_bytes);
    }
    let file = File::open(path).map_err(|error| CliError(format!("打开 {path} 失败: {error}")))?;
    let mut bytes = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| CliError(format!("读取 {path} 失败: {error}")))?;
    Ok(bytes)
}

fn read_tail(path: &str, max_bytes: u64) -> Result<String, Box<dyn std::error::Error>> {
    if path == "-" {
        return read_stdin_capped(max_bytes);
    }
    let mut file =
        File::open(path).map_err(|error| CliError(format!("打开 {path} 失败: {error}")))?;
    let len = file
        .metadata()
        .map_err(|error| CliError(format!("stat {path} 失败: {error}")))?
        .len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| CliError(format!("seek {path} 失败: {error}")))?;
    let mut bytes = Vec::new();
    // seek 后文件仍可能增长；take 保证实际 I/O 也不超过 max_bytes。
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| CliError(format!("读取 {path} 失败: {error}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_stdin_capped(max_bytes: u64) -> Result<String, Box<dyn std::error::Error>> {
    Ok(String::from_utf8_lossy(&read_stdin_bytes(max_bytes)?).into_owned())
}

fn read_stdin_bytes(max_bytes: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| CliError(format!("读取 stdin 失败: {error}")))?;
    Ok(bytes)
}

fn emit(args: &Args, text: &str) -> Result<(), Box<dyn std::error::Error>> {
    match args.value("out") {
        Some("-") | None => {
            let mut stdout = std::io::stdout();
            stdout.write_all(text.as_bytes())?;
            if !text.ends_with('\n') {
                stdout.write_all(b"\n")?;
            }
            stdout.flush()?;
        }
        Some(path) => {
            fs::write(path, text)
                .map_err(|error| CliError(format!("写入 {path} 失败: {error}")))?;
        }
    }
    Ok(())
}

fn emit_case(args: &Args, case: &DiagnosticCase) -> Result<(), Box<dyn std::error::Error>> {
    let text = if args.flag("compact") {
        serde_json::to_string(case)?
    } else {
        case.to_json_pretty()?
    };
    emit(args, &text)
}

fn exit_code(
    case: &DiagnosticCase,
    fail_on: Option<&str>,
) -> Result<u8, Box<dyn std::error::Error>> {
    let (_, warnings, errors) = case.severity_counts();
    let code = match fail_on.unwrap_or("none") {
        "warn" if warnings + errors > 0 => 2,
        "warn" => 0,
        "error" if errors > 0 => 2,
        "error" => 0,
        "none" => 0,
        other => {
            return err(format!(
                "--fail-on 需要 none|warn|error，实际 {other:?}"
            ))
        },
    };
    Ok(code)
}

fn parse_usize(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
) -> Result<usize, Box<dyn std::error::Error>> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| CliError(format!("--{name} 需要正整数，实际 {value:?}")))?;
    if parsed < min || parsed > max {
        return err(format!("--{name} 必须在 {min}..={max}"));
    }
    Ok(parsed)
}

fn print_version() {
    println!(
        "tervo-doctor {VERSION}\ncommit: {COMMIT}{}\nbuilt:  {BUILT}\ntarget: {TARGET}\nschema: tervo.diagnostic_case.v1",
        if DIRTY == "1" { " (dirty)" } else { "" }
    );
}

fn print_help() {
    print!(
        "tervo-doctor {VERSION} — 确定性、离线诊断器（不接模型/网络）\n\
         \n\
         用法:\n\
           tervo-doctor [check] [选项]        输出 tervo.diagnostic_case.v1 JSON\n\
           tervo-doctor issue [选项]          输出 issue draft markdown\n\
           tervo-doctor issue --case FILE     从已有 case 重新渲染 issue\n\
           tervo-doctor schema                输出 JSON Schema\n\
           tervo-doctor env                   输出采集到的 environment（脱敏）\n\
           tervo-doctor version | help\n\
         \n\
         输入:\n\
           --symptom TEXT  --details TEXT  --repro STEP  --expected TEXT  --actual TEXT\n\
           --diag-env FILE      `tervo diag env --json`（- 读 stdin）\n\
           --frame FILE         `tervo diag snapshot --json`\n\
           --metrics FILE       `tervo diag metrics` JSON（占位数据会被明确跳过）\n\
           --render-profile F   TERVO_RENDER_PROFILE 日志（有界解析）\n\
           --config/--profiles/--log/--log-dir PATH\n\
           --active-renderer/--requested-renderer/--fallback-reason/--no-fallback\n\
           --gpu-adapter/--gpu-backend/--gpu-driver/--gpu-device-type\n\
           --attach [LABEL=]PATH       有界、脱敏的头段附件\n\
           --attach-tail [LABEL=]PATH  有界、脱敏的日志尾段\n\
         \n\
         输出/限制:\n\
           --out FILE|-  --compact  --json（issue 时输出 case）\n\
           --max-evidence-bytes N  --max-total-bytes N  --max-evidence-items N\n\
           --fail-on none|warn|error（返回码 2 表示达到阈值）\n\
         \n\
         安全边界：不创建窗口/PTY，不读整段 scrollback，不访问网络，不调用 AI provider；\n\
         证据默认 16 KiB/项、64 KiB 总量，全部脱敏并 SHA-256 哈希；缺数据保持 null/Unknown。\n\
         库 API: tervo_diagnostics::doctor::{{Doctor, DoctorInput, DoctorConfig}}"
    );
    println!();
}
