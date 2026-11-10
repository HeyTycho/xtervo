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
        "serve" => serve(),
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
     serve                       MCP/JSON-RPC stdio server（tools: env/snapshot/replay）\n\
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
    print_json(&snapshot_value(args)?)
}

fn snapshot_value(args: &[String]) -> Result<Value, Box<dyn Error>> {
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
    Ok(frame_json(&frame, "snapshot"))
}

fn replay(args: &[String]) -> Result<(), Box<dyn Error>> {
    print_json(&replay_value(args)?)
}

fn replay_value(args: &[String]) -> Result<Value, Box<dyn Error>> {
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
    Ok(frame_json(&frame, "replay"))
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

/// 极简 MCP/JSON-RPC stdio server。非插件；随 CLI 启动，stdin/stdout 通信。
///
/// 只暴露诊断读取；snapshot 会执行调用者给的 command（这是“启动一个 shell
/// 做诊断”，不是 Tervo 远程控制）。MCP client 应只在本地显式启动本进程。
fn serve() -> Result<(), Box<dyn Error>> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[diag] MCP 坏 JSON: {e}");
                continue;
            },
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "tervo-diag", "version": VERSION},
            })),
            "tools/list" => Some(json!({"tools": mcp_tools()})),
            "tools/call" => Some(mcp_call(&req)),
            "notifications/initialized" => None,
            _ => Some(json!({"error": {"code": -32601, "message": "method not found"}})),
        };
        let Some(mut result) = response else { continue };
        if let Some(id) = id {
            let mut envelope = json!({"jsonrpc": "2.0", "id": id});
            if let Some(error) = result.get("error").cloned() {
                envelope["error"] = error;
            } else {
                envelope["result"] = result;
            }
            writeln!(stdout, "{}", serde_json::to_string(&envelope)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn mcp_tools() -> Value {
    json!([
        {
            "name": "tervo.diag_env",
            "description": "Tervo version/display/config/log paths; no env values or secrets.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "tervo.diag_snapshot",
            "description": "Run a shell command in a one-shot PTY and return the terminal frame/cursor/images metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "shell -c command"},
                    "shell": {"type": "string", "default": "/bin/sh"},
                    "cols": {"type": "integer", "default": 120},
                    "rows": {"type": "integer", "default": 32},
                    "wait_ms": {"type": "integer", "default": 250}
                },
                "required": ["command"]
            }
        },
        {
            "name": "tervo.diag_replay",
            "description": "Replay raw terminal bytes from a file through the real PTY/core and return a frame.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "input": {"type": "string"},
                    "cols": {"type": "integer", "default": 120},
                    "rows": {"type": "integer", "default": 32},
                    "wait_ms": {"type": "integer", "default": 400}
                },
                "required": ["input"]
            }
        }
    ])
}

fn mcp_call(req: &Value) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let result = match name {
        "tervo.diag_env" => Ok(env_json()),
        "tervo.diag_snapshot" => {
            let command = match args.get("command").and_then(Value::as_str) {
                Some(c) => c.to_string(),
                None => return mcp_error(-32602, "command is required"),
            };
            let mut cli = Vec::new();
            if let Some(v) = args.get("cols").and_then(Value::as_u64) {
                cli.extend(["--cols".into(), v.to_string()]);
            }
            if let Some(v) = args.get("rows").and_then(Value::as_u64) {
                cli.extend(["--rows".into(), v.to_string()]);
            }
            if let Some(v) = args.get("wait_ms").and_then(Value::as_u64) {
                cli.extend(["--wait-ms".into(), v.to_string()]);
            }
            cli.extend([
                "--shell".into(),
                args.get("shell").and_then(Value::as_str).unwrap_or("/bin/sh").to_string(),
                "--".into(),
                "-c".into(),
                command,
            ]);
            snapshot_value(&cli)
        },
        "tervo.diag_replay" => {
            let input = match args.get("input").and_then(Value::as_str) {
                Some(v) => v.to_string(),
                None => return mcp_error(-32602, "input is required"),
            };
            let mut cli = vec!["--input".into(), input];
            if let Some(v) = args.get("cols").and_then(Value::as_u64) {
                cli.extend(["--cols".into(), v.to_string()]);
            }
            if let Some(v) = args.get("rows").and_then(Value::as_u64) {
                cli.extend(["--rows".into(), v.to_string()]);
            }
            if let Some(v) = args.get("wait_ms").and_then(Value::as_u64) {
                cli.extend(["--wait-ms".into(), v.to_string()]);
            }
            replay_value(&cli)
        },
        _ => return mcp_error(-32602, "unknown tool"),
    };
    match result {
        Ok(value) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("Tervo diag error: {e}")}],
            "isError": true,
        }),
    }
}

fn mcp_error(code: i64, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_exposes_read_only_diag_tools() {
        let tools = mcp_tools();
        let names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"tervo.diag_env"));
        assert!(names.contains(&"tervo.diag_snapshot"));
        assert!(names.contains(&"tervo.diag_replay"));
    }

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
