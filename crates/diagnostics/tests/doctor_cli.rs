// SPDX-License-Identifier: GPL-3.0-only
//! Smoke tests for the `tervo-doctor` binary. The process is launched with a
//! cleared environment (no DISPLAY/WAYLAND_DISPLAY), proving the slice never
//! needs :1 or a window.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tervo-doctor-cli-{}-{}", name, std::process::id()));
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

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tervo-doctor"));
    command
        .env_clear()
        .env("HOME", dir.path.as_os_str())
        .env("TERVO_PROFILES", dir.join("profiles.toml"))
        .env("TERVO_LOG_DIR", dir.join("logs"));
    command
}

fn run(mut command: Command) -> Output {
    command.output().expect("spawn tervo-doctor")
}

fn stdout_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout is JSON")
}

#[test]
fn check_works_without_display_and_redacts_attachment() {
    let dir = TempDir::new("check");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let frame = dir.write(
        "frame.json",
        r#"{"schema":"tervo.diag.v1","kind":"snapshot","cols":80,"rows":2,"generation":2,"damage":{"full":false,"region_count":1,"rects":[{"x":0,"y":0,"w":10,"h":1}]},"images":[],"text":["hello","world"]}"#,
    );
    let profile = dir.write("profile.log", "frame_ms=12.5\ndraw_ms=3.0\n");
    let log = dir.write("app.log", "ready\napi_key=sk-live-1234567890\n");
    let mut cmd = command(&dir);
    cmd.args([
        "check",
        "--symptom",
        "滚动后残影",
        "--config",
        config.to_str().unwrap(),
        "--frame",
        frame.to_str().unwrap(),
        "--render-profile",
        profile.to_str().unwrap(),
        "--attach-tail",
        &format!("app={}", log.display()),
        "--compact",
    ]);
    let output = run(cmd);
    let value = stdout_json(&output);
    assert_eq!(value["schema"], "tervo.diagnostic_case.v1");
    assert!(value["environment"]["display"].is_null());
    assert!(value["environment"]["wayland_display"].is_null());
    assert!(!value["probes"].as_array().unwrap().is_empty());
    assert!(!value["findings"].as_array().unwrap().is_empty());
    assert_eq!(value["evidence"]["sha256"].as_str().unwrap().len(), 64);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("sk-live-1234567890"));
    assert!(text.contains("tervo.diagnostic_case.v1"));
}

#[test]
fn schema_and_issue_commands_are_usable() {
    let dir = TempDir::new("schema-issue");
    let config = dir.write("config.toml", "theme = \"dark\"\n");
    let frame = dir.write(
        "frame.json",
        r#"{"schema":"tervo.diag.v1","kind":"snapshot","cols":40,"rows":2,"generation":2,"damage":{"full":false,"region_count":1,"rects":[{"x":0,"y":0,"w":10,"h":1}]},"images":[],"text":["a","b"]}"#,
    );
    let case_path = dir.join("case.json");

    let mut schema_cmd = command(&dir);
    schema_cmd.arg("schema");
    let schema_value = stdout_json(&run(schema_cmd));
    assert_eq!(
        schema_value["$id"],
        "https://tervo.dev/schemas/tervo.diagnostic_case.v1.json"
    );
    assert_eq!(
        schema_value["properties"]["schema"]["const"],
        "tervo.diagnostic_case.v1"
    );

    let mut check = command(&dir);
    check.args([
        "check",
        "--config",
        config.to_str().unwrap(),
        "--frame",
        frame.to_str().unwrap(),
        "--symptom",
        "花屏",
        "--out",
        case_path.to_str().unwrap(),
        "--compact",
    ]);
    let output = run(check);
    assert!(
        output.status.success(),
        "check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(case_path.exists());

    let mut issue = command(&dir);
    issue.args(["issue", "--case", case_path.to_str().unwrap()]);
    let output = run(issue);
    assert!(output.status.success());
    let markdown = String::from_utf8(output.stdout).unwrap();
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
        assert!(markdown.contains(heading), "missing {heading}");
    }
}

#[test]
fn fail_on_error_uses_exit_code_2() {
    let dir = TempDir::new("fail-on");
    let config = dir.write(
        "config.toml",
        "theme = \"dark\"\ntoken = \"ghp_abcdefghijklmnopqrstuvwxyz0123456789\"\n",
    );
    let mut cmd = command(&dir);
    cmd.args([
        "check",
        "--config",
        config.to_str().unwrap(),
        "--fail-on",
        "error",
        "--compact",
    ]);
    let output = run(cmd);
    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(value["schema"], "tervo.diagnostic_case.v1");
}

#[test]
fn missing_input_file_is_a_clean_error() {
    let dir = TempDir::new("missing-input");
    let mut cmd = command(&dir);
    cmd.args(["check", "--frame", "/definitely/not/a/frame.json"]);
    let output = run(cmd);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("打开"));
}
