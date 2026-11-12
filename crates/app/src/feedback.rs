// SPDX-License-Identifier: GPL-3.0-only
//! User-initiated feedback bundles.
//!
//! A bundle is deliberately boring and bounded: version/build metadata, a few
//! environment facts, redacted config/log tails, and the user's description.
//! It never contains scrollback, terminal contents, environment values, Vault
//! data, or API keys. Creating a bundle is local; opening an issue URL is the
//! only optional network-adjacent action and is always an explicit click.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;

use crate::config::Config;

const MAX_LOG_FILES: usize = 8;
const MAX_LOG_BYTES: u64 = 128 * 1024;
const MAX_CONFIG_BYTES: u64 = 128 * 1024;

fn now_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn read_tail(path: &Path, max: u64) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return String::new(),
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max {
        use std::io::{Seek, SeekFrom};
        if file.seek(SeekFrom::Start(len - max)).is_err() {
            return String::new();
        }
    }
    let mut bytes = Vec::new();
    let _ = file.take(max).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).to_string()
}

fn redact(input: &str) -> String {
    tervo_diagnostics::doctor::redact_text(input).0
}

/// Create a redacted `.tar.gz` under the state feedback directory.
pub fn create_bundle(description: &str) -> Result<PathBuf, String> {
    let base = crate::logging::dir();
    let root = base
        .parent()
        .map(|p| p.join("feedback"))
        .unwrap_or_else(|| base.join("feedback"));
    fs::create_dir_all(&root).map_err(|e| format!("create feedback dir: {e}"))?;
    let stamp = now_stamp();
    let stage = root.join(format!(".stage-{stamp}"));
    fs::create_dir_all(stage.join("logs")).map_err(|e| format!("create stage: {e}"))?;

    let env = json!({
        "schema": "tervo.feedback.v1",
        "version": crate::BUILD_VERSION,
        "commit": crate::BUILD_COMMIT,
        "target": crate::BUILD_TARGET,
        "built": crate::BUILD_TIME,
        "display": std::env::var("DISPLAY").ok(),
        "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
        "session_type": std::env::var("XDG_SESSION_TYPE").ok(),
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "config_path": Config::path().display().to_string(),
        "logs_path": base.display().to_string(),
    });
    fs::write(
        stage.join("env.json"),
        serde_json::to_vec_pretty(&env).map_err(|e| format!("encode env: {e}"))?,
    )
    .map_err(|e| format!("write env: {e}"))?;

    let report = format!(
        "# Tervo feedback\n\n\
         Version: {} ({})\n\
         Built: {}\n\
         User description:\n\n{}\n\n\
         This bundle contains no terminal contents, environment values, Vault data, or API keys.\n",
        crate::BUILD_VERSION,
        crate::BUILD_COMMIT,
        crate::BUILD_TIME,
        if description.trim().is_empty() {
            "(not provided)".to_string()
        } else {
            redact(description.trim())
        }
    );
    fs::write(stage.join("report.md"), report).map_err(|e| format!("write report: {e}"))?;

    let config_path = Config::path();
    if config_path.is_file() {
        let text = redact(&read_tail(&config_path, MAX_CONFIG_BYTES));
        fs::write(stage.join("config.redacted.toml"), text)
            .map_err(|e| format!("write config: {e}"))?;
    }

    let mut logs = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "log") {
                logs.push(path);
            }
        }
    }
    logs.sort_by_key(|p| p.metadata().ok().and_then(|m| m.modified().ok()));
    for (index, path) in logs.into_iter().rev().take(MAX_LOG_FILES).enumerate() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tervo.log");
        let text = redact(&read_tail(&path, MAX_LOG_BYTES));
        fs::write(stage.join("logs").join(format!("{index}-{name}")), text)
            .map_err(|e| format!("write log: {e}"))?;
    }

    let bundle = root.join(format!("tervo-feedback-{stamp}.tar.gz"));
    let file = File::create(&bundle).map_err(|e| format!("create bundle: {e}"))?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(encoder);
    tar.append_dir_all("feedback", &stage)
        .map_err(|e| format!("tar bundle: {e}"))?;
    tar.finish().map_err(|e| format!("finish bundle: {e}"))?;
    let _ = fs::remove_dir_all(&stage);
    Ok(bundle)
}

fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Open a configured issue URL with a short prefilled title/body.
/// Never posts an issue itself and never receives/reads a token.
pub fn open_issue(url: &str, bundle: &Path) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("feedback issue URL is empty".into());
    }
    let title = format!("Tervo feedback: {}", crate::BUILD_VERSION);
    let body = format!(
        "Tervo {} ({})\n\nAttach the generated bundle: {}\n\nPlease describe reproduction steps.",
        crate::BUILD_VERSION,
        crate::BUILD_COMMIT,
        bundle.display()
    );
    let sep = if url.contains('?') { '&' } else { '?' };
    let full = format!(
        "{url}{sep}title={}&description={}",
        percent_encode(&title),
        percent_encode(&body)
    );
    std::process::Command::new("xdg-open")
        .arg(&full)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("xdg-open failed: {e}"))
}

pub fn open_folder(path: &Path) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("xdg-open failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn bundle_is_bounded_and_redacts_description() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("tervo-feedback-test-{}", now_stamp()));
        let _ = fs::create_dir_all(&dir);
        let old = std::env::var_os("TERVO_LOG_DIR");
        std::env::set_var("TERVO_LOG_DIR", &dir);
        fs::write(dir.join("tervo-1.log"), "api_key=SUPERSECRET\nplain line\n").unwrap();
        let bundle = create_bundle("my api_key=SUPERSECRET").expect("bundle");
        let file = File::open(&bundle).expect("open bundle");
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut found_report = false;
        for entry in archive.entries().expect("tar entries") {
            let mut entry = entry.expect("entry");
            let path = entry.path().expect("path").display().to_string();
            if path.ends_with("report.md") {
                found_report = true;
                let mut text = String::new();
                entry.read_to_string(&mut text).expect("read report");
                assert!(!text.contains("SUPERSECRET"), "description must be redacted");
                assert!(text.contains("api_key=<redacted>") || text.contains("api_key"));
            }
        }
        assert!(found_report, "report.md missing from bundle");
        let _ = fs::remove_file(&bundle);
        let _ = fs::remove_dir_all(&dir);
        match old {
            Some(value) => std::env::set_var("TERVO_LOG_DIR", value),
            None => std::env::remove_var("TERVO_LOG_DIR"),
        }
    }
}
