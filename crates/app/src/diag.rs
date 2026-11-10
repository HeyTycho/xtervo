// SPDX-License-Identifier: GPL-3.0-only
//! 内置 harness-neutral 终端诊断 CLI（`tervo diag`）。
//!
//! 不创建窗口、不初始化 Slint；输出 `tervo.diag.v1` JSON，任何 harness 都可以
//! 在 shell 里调用。当前提供 env / snapshot / replay（无实时 app socket、无 MCP；
//! 那是后续切片）。
//!
//! 安全：不输出环境变量值、API key、Vault 内容或图片 RGBA；只给版本、路径、
//! frame 文本、cursor/modes 和图片 rect。

use std::error::Error;
use std::time::Duration;

use serde_json::{json, Value};
use tervo_core::{spawn, Frame, PaneConfig};

use crate::config::Config;
use crate::logging;
use crate::profile;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("TERVO_GIT_COMMIT");
const DIRTY: &str = env!("TERVO_GIT_DIRTY");
const BUILT: &str = env!("TERVO_BUILD_TIME");
const TARGET: &str = env!("TERVO_BUILD_TARGET");

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let sub = args.first().map(String::as_str).unwrap_or("env");
    match sub {
        "env" | "--json" => {
            print_json(&env_json())?;
            Ok(())
        },
        "snapshot" => snapshot(&args[1..]),
        "replay" => replay(&args[1..]),
        "trace" => trace(&args[1..]),
        "help" | "-h" | "--help" => {
            println!("{}", help_text());
            Ok(())
        },
        other => Err(format!("未知 diag 子命令: {other}（用 tervo diag help）").into()),
    }
}

fn help_text() -> &'static str {
    "用法: tervo diag [env|snapshot|replay] [选项]\n\
     \n\
     env                         环境/版本/路径 JSON（默认）\n\
     snapshot                    启动一次性 pane，抓当前 frame JSON\n\
       --cols N                  列数（默认 120）\n\
       --rows N                  行数（默认 32）\n\
       --wait-ms N               等待输出毫秒（默认 250）\n\
       --shell PATH              默认 /bin/bash\n\
       -- <args...>              传给 shell 的参数\n\
     replay --input FILE         把录制字节通过 PTY 回放并抓 frame\n\
     trace --input FILE --out F  回放并按 frame 变化写 JSONL trace\n\
       --duration-ms N           默认 1000\n\
       --cols/--rows/--wait-ms\n\
     \n\
     输出 schema=tervo.diag.v1；不含 API key/环境变量值。"
}

fn opt(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn num(args: &[String], flag: &str, default: usize) -> usize {
    opt(args, flag)
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, 10_000)
}

fn wait_ms(args: &[String], default: u64) -> u64 {
    opt(args, "--wait-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
        .min(30_000)
}

fn extra_args(args: &[String]) -> Vec<String> {
    args.iter()
        .position(|a| a == "--")
        .map(|i| args[i + 1..].to_vec())
        .unwrap_or_default()
}

fn shell(args: &[String]) -> String {
    opt(args, "--shell").unwrap_or_else(|| "/bin/bash".to_string())
}

fn print_json(value: &Value) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn env_json() -> Value {
    json!({
        "schema": "tervo.diag.v1",
        "kind": "env",
        "version": VERSION,
        "commit": COMMIT,
        "dirty": DIRTY == "1",
        "built": BUILT,
        "target": TARGET,
        "pid": std::process::id(),
        "display": std::env::var("DISPLAY").ok(),
        "wayland_display": std::env::var("WAYLAND_DISPLAY").ok(),
        "session_type": std::env::var("XDG_SESSION_TYPE").ok(),
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "backend": std::env::var("SLINT_BACKEND").ok(),
        "dri": std::path::Path::new("/dev/dri").exists(),
        "config": Config::path().display().to_string(),
        "profiles": profile::ProfileStore::path().display().to_string(),
        "log": logging::path().map(|p| p.display().to_string()),
    })
}

fn snapshot(args: &[String]) -> Result<(), Box<dyn Error>> {
    let cfg = PaneConfig {
        cols: num(args, "--cols", 120) as u16,
        rows: num(args, "--rows", 32) as u16,
        shell: Some(shell(args)),
        args: extra_args(args),
        ..Default::default()
    };
    let handle = spawn(cfg)?;
    std::thread::sleep(Duration::from_millis(wait_ms(args, 250)));
    let frame = handle.pull(handle.new_frame());
    print_json(&frame_json(&frame, "snapshot"))
}

fn replay(args: &[String]) -> Result<(), Box<dyn Error>> {
    let input = opt(args, "--input").ok_or("replay 需要 --input FILE")?;
    // pane 的 cwd 不一定等于调用者 cwd；先转绝对路径，避免 cat 找不到文件。
    let input = std::fs::canonicalize(&input)
        .map_err(|e| format!("replay 输入文件不存在/不可读: {input}: {e}"))?
        .display()
        .to_string();
    let cfg = PaneConfig {
        cols: num(args, "--cols", 120) as u16,
        rows: num(args, "--rows", 32) as u16,
        shell: Some("/bin/cat".to_string()),
        args: vec![input],
        ..Default::default()
    };
    let handle = spawn(cfg)?;
    std::thread::sleep(Duration::from_millis(wait_ms(args, 400)));
    let frame = handle.pull(handle.new_frame());
    print_json(&frame_json(&frame, "replay"))
}

fn trace(args: &[String]) -> Result<(), Box<dyn Error>> {
    let input = opt(args, "--input").ok_or("trace 需要 --input FILE")?;
    let input = std::fs::canonicalize(&input)
        .map_err(|e| format!("trace 输入文件不存在/不可读: {input}: {e}"))?
        .display()
        .to_string();
    let out_path = opt(args, "--out").ok_or("trace 需要 --out FILE")?;
    let duration = opt(args, "--duration-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1000)
        .clamp(50, 30_000);
    let cfg = PaneConfig {
        cols: num(args, "--cols", 120) as u16,
        rows: num(args, "--rows", 32) as u16,
        shell: Some("/bin/cat".to_string()),
        args: vec![input.clone()],
        ..Default::default()
    };
    let handle = spawn(cfg)?;
    let start = std::time::Instant::now();
    let mut seq = 0u64;
    let mut last_gen = u64::MAX;
    let mut lines = Vec::new();
    let mut final_value: Option<Value> = None;
    while start.elapsed() < Duration::from_millis(duration) {
        std::thread::sleep(Duration::from_millis(25));
        let frame = handle.pull(handle.new_frame());
        if frame.generation == last_gen {
            continue;
        }
        last_gen = frame.generation;
        seq += 1;
        let mut value = frame_json(&frame, "trace");
        value["seq"] = json!(seq);
        value["ts_ms"] = json!(start.elapsed().as_millis() as u64);
        value["input"] = json!(input);
        lines.push(serde_json::to_string(&value)?);
        final_value = Some(value);
    }
    if lines.is_empty() {
        let frame = handle.pull(handle.new_frame());
        seq += 1;
        let mut value = frame_json(&frame, "trace");
        value["seq"] = json!(seq);
        value["ts_ms"] = json!(start.elapsed().as_millis() as u64);
        value["input"] = json!(input);
        lines.push(serde_json::to_string(&value)?);
        final_value = Some(value);
    }
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(&out_path, body)?;
    eprintln!(
        "[diag] trace {} frames -> {} ({} ms, last gen {:?})",
        seq, out_path, duration, final_value.as_ref().and_then(|v| v.get("generation"))
    );
    Ok(())
}

/// 稳定 schema v1 的 frame 部分；图片只给 rect，不给 RGBA。
fn frame_json(frame: &Frame, kind: &str) -> Value {
    let text: Vec<String> = (0..frame.rows)
        .map(|r| {
            frame
                .row(r)
                .iter()
                .map(|c| c.char())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    let images: Vec<Value> = frame
        .images
        .iter()
        .map(|img| {
            json!({
                "id": img.id,
                "row": img.row,
                "col": img.col,
                "width": img.width,
                "height": img.height,
            })
        })
        .collect();
    json!({
        "schema": "tervo.diag.v1",
        "kind": kind,
        "version": VERSION,
        "commit": COMMIT,
        "target": TARGET,
        "cols": frame.cols,
        "rows": frame.rows,
        "cursor": {
            "row": frame.cursor.line,
            "col": frame.cursor.col,
            "visible": frame.cursor.visible,
            "shape": format!("{:?}", frame.cursor.shape).to_lowercase(),
        },
        "mode": frame.mode,
        "display_offset": frame.display_offset,
        "generation": frame.generation,
        "images": images,
        "text": text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_schema_is_stable_and_omits_image_bytes() {
        let mut frame = Frame::new(4, 1);
        frame.cells[0].ch = 'A' as u32;
        frame.cursor.line = 0;
        frame.cursor.col = 1;
        let value = frame_json(&frame, "snapshot");
        assert_eq!(value["schema"], "tervo.diag.v1");
        assert_eq!(value["kind"], "snapshot");
        assert_eq!(value["cols"], 4);
        assert_eq!(value["cursor"]["col"], 1);
        assert_eq!(value["text"][0], "A");
        assert!(value.get("data").is_none());
    }
}
