// SPDX-License-Identifier: GPL-3.0-only
//! Read-only MCP tools layered on the existing `xtervo diag serve` server.
//!
//! Everything here is deliberately boring: bounded file reads, no process
//! spawning, no writes, and no raw log lines or terminal content in the output.
//! JSON inputs may be inline objects or paths to files under `$HOME`/`/tmp`;
//! paths outside those roots are rejected. The existing `xtervo.diag_snapshot`
//! tool is the only MCP tool that starts a shell, and that behavior is
//! unchanged.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use xtervo_diagnostics::doctor::{
    issue::render_issue, DiagnosticCase, Doctor, DoctorConfig, DoctorInput, Environment,
    FrameSummary, MetricsSummary, Symptom,
};

/// Upper bound for `environment`/`frame`/`metrics` JSON files and inline JSON.
const MAX_INPUT_JSON_BYTES: u64 = 1024 * 1024;
const MAX_PATH_CHARS: usize = 4096;
const MAX_DIR_ENTRIES: usize = 512;
const MAX_LOG_FILES: usize = 8;
const MAX_LOG_SCAN_BYTES: u64 = 256 * 1024;

/// Run the deterministic doctor over optional environment/frame/metrics input.
///
/// The output is the versioned `xtervo.diagnostic_case.v1` document, including
/// its issue draft. Missing inputs stay absent; the doctor never invents a
/// healthy zero. Only files under `$HOME` or `/tmp` may be referenced by path.
pub(super) fn doctor_check(args: &Value) -> Result<Value, String> {
    let args = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;

    let environment = match args.get("environment") {
        None | Some(Value::Null) => Environment::from_process(),
        Some(value) => parse_environment(value)?,
    };
    validate_environment_paths(&environment)?;
    let frame = match args.get("frame") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let value = read_json_input(value, "frame")?;
            Some(
                FrameSummary::from_diag_json(&value)
                    .map_err(|error| format!("invalid frame: {error}"))?,
            )
        },
    };
    let metrics = match args.get("metrics") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let value = read_json_input(value, "metrics")?;
            Some(
                MetricsSummary::from_diag_json(&value)
                    .map_err(|error| format!("invalid metrics: {error}"))?,
            )
        },
    };
    let symptom = symptom_from_args(args)?;

    let input = DoctorInput {
        symptom,
        environment,
        frame,
        metrics,
        render_profile: None,
        attachments: Vec::new(),
    };
    let case = Doctor::new(DoctorConfig::default())
        .run(&input)
        .map_err(|error| format!("doctor check failed: {error}"))?;
    case.to_json()
        .map_err(|error| format!("serialize diagnostic case: {error}"))
}

/// Render the fixed doctor issue template from a full diagnostic case.
///
/// The case must already be a validated `xtervo.diagnostic_case.v1` document
/// (for example the output of `xtervo.doctor_check`). No model or network is
/// involved; the markdown is deterministic.
pub(super) fn issue_draft(args: &Value) -> Result<Value, String> {
    let args = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    let case_value = args
        .get("case")
        .ok_or_else(|| "case is required".to_string())?;
    if !case_value.is_object() {
        return Err("case must be a JSON object".to_string());
    }
    let text = serde_json::to_string(case_value)
        .map_err(|error| format!("serialize case: {error}"))?;
    let case = DiagnosticCase::from_json(&text)
        .map_err(|error| format!("invalid diagnostic case: {error}"))?;
    let draft = render_issue(&case);
    Ok(json!({
        "schema": "xtervo.diag.v1",
        "kind": "issue_draft",
        "read_only": true,
        "case_id": case.case_id,
        "title": draft.title,
        "labels": draft.labels,
        "markdown": draft.markdown,
    }))
}

/// Bounded summary of the xtervo log directory.
///
/// Only regular `xtervo-*.log` files are considered. At most eight files are
/// reported, and each file is scanned only at its tail (256 KiB) for line and
/// error-marker counts. Raw lines are never returned, so terminal output or
/// accidental secrets in a log do not leave this process.
pub(super) fn metrics_read(args: &Value) -> Result<Value, String> {
    let args = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    let explicit = match args.get("log_dir") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err("log_dir must be a string".to_string()),
    };

    let (log_dir, exists) = match explicit {
        Some(raw) => (resolve_allowed_dir(raw)?, true),
        None => {
            let dir = crate::logging::dir();
            let raw = dir
                .to_str()
                .ok_or_else(|| "default log dir is not valid UTF-8".to_string())?;
            if dir.exists() {
                (resolve_allowed_dir(raw)?, true)
            } else {
                validate_allowed_env_path("log_dir", raw)?;
                (dir, false)
            }
        },
    };

    let mut stats = Vec::new();
    let mut matched_files = 0usize;
    let mut entry_errors = 0usize;
    let mut dir_entries_truncated = false;

    if exists {
        let entries = fs::read_dir(&log_dir)
            .map_err(|error| format!("read log_dir {}: {error}", log_dir.display()))?;
        for (index, entry) in entries.enumerate() {
            if index >= MAX_DIR_ENTRIES {
                dir_entries_truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                entry_errors += 1;
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !(name.starts_with("xtervo-") && name.ends_with(".log")) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                entry_errors += 1;
                continue;
            };
            // Exclude symlinks and directories: only app-created regular logs.
            if !file_type.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                entry_errors += 1;
                continue;
            };
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            matched_files += 1;
            stats.push((name.to_string(), metadata.len(), modified));
        }
    }

    // Newest first, then by name so the same directory yields the same summary.
    stats.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| left.0.cmp(&right.0))
    });
    stats.truncate(MAX_LOG_FILES);

    let mut files = Vec::with_capacity(stats.len());
    let mut total_bytes = 0u64;
    let mut scanned_bytes = 0u64;
    let mut lines = 0u64;
    let mut recent_errors = 0u64;
    let mut scan_truncated = false;

    for (name, bytes, modified) in stats {
        total_bytes = total_bytes.saturating_add(bytes);
        let (file_scanned, file_lines, file_errors, truncated) =
            scan_log_tail(&log_dir.join(&name), bytes)?;
        scanned_bytes = scanned_bytes.saturating_add(file_scanned);
        lines = lines.saturating_add(file_lines);
        recent_errors = recent_errors.saturating_add(file_errors);
        scan_truncated |= truncated;
        files.push(json!({
            "name": name,
            "bytes": bytes,
            "scanned_bytes": file_scanned,
            "lines": file_lines,
            "recent_errors": file_errors,
            "truncated": truncated,
            "modified_unix": modified,
        }));
    }

    Ok(json!({
        "schema": "xtervo.diag.v1",
        "kind": "metrics_read",
        "read_only": true,
        "log_dir": log_dir.display().to_string(),
        "exists": exists,
        "limits": {
            "max_files": MAX_LOG_FILES,
            "max_bytes_per_file": MAX_LOG_SCAN_BYTES,
            "max_dir_entries": MAX_DIR_ENTRIES,
        },
        "summary": {
            "matched_files": matched_files,
            "stats_files": files.len(),
            "bytes": total_bytes,
            "scanned_bytes": scanned_bytes,
            "lines": lines,
            "recent_errors": recent_errors,
            "truncated": scan_truncated,
            "entry_errors": entry_errors,
            "dir_entries_truncated": dir_entries_truncated,
        },
        "files": files,
    }))
}

/// MCP resource descriptor list. M0 exposes only the local build/version
/// document; it does not expose terminal content or live app state.
pub(super) fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": "xtervo://version",
                "name": "xtervo version and build metadata",
                "description": "Version, commit, build time, target and renderer environment for this xtervo diag process. Read-only; no secrets.",
                "mimeType": "application/json"
            }
        ]
    })
}

/// Read one of the static resources. Unknown URIs are an error, not an empty
/// success, so a client can tell the difference between "missing" and "empty".
pub(super) fn read_resource(uri: &str) -> Result<Value, String> {
    match uri {
        "xtervo://version" => {
            let text = serde_json::to_string_pretty(&super::env_json())
                .map_err(|error| format!("serialize version resource: {error}"))?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        },
        _ => Err(format!("unknown resource: {uri}")),
    }
}

fn parse_environment(value: &Value) -> Result<Environment, String> {
    let value = read_json_input(value, "environment")?;
    let object = value
        .as_object()
        .ok_or_else(|| "environment must be a JSON object or a path to one".to_string())?;
    // Accept both the app's `xtervo diag env --json` document and the
    // doctor-internal Environment shape. Prefer the documented diag form when
    // either marker is present.
    let structured = object.contains_key("config_path")
        || object.contains_key("profiles_path")
        || object.contains_key("build_time")
        || object.contains_key("dri_present");
    if structured {
        return serde_json::from_value(value)
            .map_err(|error| format!("invalid environment: {error}"));
    }
    Environment::from_diag_env_json(&value)
        .map_err(|error| format!("invalid diagnostic environment: {error}"))
}

/// Environment objects can carry paths too. Validate them before the doctor
/// opens anything so an inline object cannot smuggle `/etc/shadow` past the
/// path allowlist. Missing paths under an allowed root are still valid input;
/// the doctor reports them as absent rather than reading them.
fn validate_environment_paths(environment: &Environment) -> Result<(), String> {
    for (label, path) in [
        ("config_path", environment.config_path.as_deref()),
        ("profiles_path", environment.profiles_path.as_deref()),
        ("log_path", environment.log_path.as_deref()),
        ("log_dir", environment.log_dir.as_deref()),
    ] {
        if let Some(raw) = path {
            validate_allowed_env_path(label, raw)?;
        }
    }
    Ok(())
}

fn validate_allowed_env_path(label: &str, raw: &str) -> Result<(), String> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    if raw.chars().count() > MAX_PATH_CHARS {
        return Err(format!("{label} is longer than {MAX_PATH_CHARS} characters"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(format!("{label} must be absolute: {raw}"));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("{label} must not contain '..': {raw}"));
    }
    let resolved = if path.exists() {
        fs::canonicalize(path).map_err(|error| format!("resolve {label} {raw}: {error}"))?
    } else {
        nearest_existing_ancestor(path)
            .map_err(|error| format!("resolve {label} ancestor for {raw}: {error}"))?
    };
    ensure_allowed(&resolved)
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut current = path;
    loop {
        if current.exists() {
            return fs::canonicalize(current)
                .map_err(|error| format!("canonicalize {}: {error}", current.display()));
        }
        current = current
            .parent()
            .ok_or_else(|| format!("no existing ancestor for {}", path.display()))?;
    }
}

fn symptom_from_args(args: &serde_json::Map<String, Value>) -> Result<Symptom, String> {
    let Some(summary) = optional_string(args, "symptom")? else {
        return Ok(Symptom::default());
    };
    let mut symptom = Symptom::reported(summary);
    if let Some(details) = optional_string(args, "details")? {
        symptom = symptom.with_details(details);
    }
    if let Some(value) = args.get("repro_steps") {
        let steps = value
            .as_array()
            .ok_or_else(|| "repro_steps must be an array of strings".to_string())?
            .iter()
            .map(|step| {
                step.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "repro_steps must be an array of strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !steps.is_empty() {
            symptom = symptom.with_repro_steps(steps);
        }
    }
    if let (Some(expected), Some(actual)) = (
        optional_string(args, "expected")?,
        optional_string(args, "actual")?,
    ) {
        symptom = symptom.with_expected_actual(expected, actual);
    }
    Ok(symptom)
}

fn optional_string(
    args: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{name} must be a string")),
    }
}

fn read_json_input(value: &Value, label: &str) -> Result<Value, String> {
    match value {
        Value::Object(_) => Ok(value.clone()),
        Value::String(path) => {
            let path = resolve_allowed_file(path)?;
            let text = read_text_bounded(&path, MAX_INPUT_JSON_BYTES)?;
            serde_json::from_str(&text)
                .map_err(|error| format!("{label} file is not JSON: {error}"))
        },
        _ => Err(format!("{label} must be a JSON object or a path string")),
    }
}

fn resolve_allowed_file(raw: &str) -> Result<PathBuf, String> {
    let path = resolve_allowed_path(raw)?;
    let metadata =
        fs::metadata(&path).map_err(|error| format!("stat path {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("path is not a regular file: {}", path.display()));
    }
    Ok(path)
}

fn resolve_allowed_dir(raw: &str) -> Result<PathBuf, String> {
    let path = resolve_allowed_path(raw)?;
    let metadata =
        fs::metadata(&path).map_err(|error| format!("stat path {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("path is not a directory: {}", path.display()));
    }
    Ok(path)
}

fn resolve_allowed_path(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.chars().count() > MAX_PATH_CHARS {
        return Err(format!("path is longer than {MAX_PATH_CHARS} characters"));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("path must be absolute".to_string());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("path must not contain '..'".to_string());
    }
    let resolved =
        fs::canonicalize(path).map_err(|error| format!("resolve path {raw}: {error}"))?;
    ensure_allowed(&resolved)?;
    Ok(resolved)
}

fn ensure_allowed(path: &Path) -> Result<(), String> {
    let roots = allowed_roots();
    if roots.is_empty() {
        return Err("HOME and /tmp are unavailable; refusing to read a path".to_string());
    }
    if roots.iter().any(|root| path.starts_with(root)) {
        Ok(())
    } else {
        Err(format!(
            "path is outside the allowed roots ($HOME or /tmp): {}",
            path.display()
        ))
    }
}

fn allowed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(path) = fs::canonicalize(PathBuf::from(home)) {
            roots.push(path);
        }
    }
    if let Ok(path) = fs::canonicalize("/tmp") {
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots
}

fn read_text_bounded(path: &Path, max_bytes: u64) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if metadata.len() > max_bytes {
        return Err(format!(
            "file {} is larger than the {max_bytes}-byte limit",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "file {} is larger than the {max_bytes}-byte limit",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("file {} is not UTF-8", path.display()))
}

fn scan_log_tail(path: &Path, bytes: u64) -> Result<(u64, u64, u64, bool), String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let truncated = bytes > MAX_LOG_SCAN_BYTES;
    let start = bytes.saturating_sub(MAX_LOG_SCAN_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .map_err(|error| format!("seek {}: {error}", path.display()))?;
    }
    let mut buffer = Vec::new();
    file.take(MAX_LOG_SCAN_BYTES)
        .read_to_end(&mut buffer)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = 0u64;
    let mut errors = 0u64;
    for line in text.split_inclusive('\n') {
        if line.is_empty() {
            continue;
        }
        lines = lines.saturating_add(1);
        if is_error_line(line) {
            errors = errors.saturating_add(1);
        }
    }
    Ok((buffer.len() as u64, lines, errors, truncated))
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("panic") || lower.contains("fatal")
}
