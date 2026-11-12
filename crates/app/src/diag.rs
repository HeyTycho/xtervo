// SPDX-License-Identifier: GPL-3.0-only
//! 内置 harness-neutral 终端诊断 CLI（`tervo diag`）。
//!
//! 不创建窗口、不初始化 Slint；输出 `tervo.diag.v1` JSON，任何 harness 都可以
//! 在 shell 里调用。当前提供 env / snapshot / replay（无实时 app socket、无 MCP；
//! 那是后续切片）。
//!
//! 安全：不输出环境变量值、API key、Vault 内容或图片 RGBA；只给版本、路径、
//! frame 文本、cursor/modes 和图片 rect。

mod trace_layer;

use std::error::Error;
use std::time::Duration;

use serde_json::{json, Value};
use tervo_core::{spawn, Frame, PaneConfig};
use tervo_diagnostics::{
    AtomicResourceTracker, DamageRect, DamageSummary, ImageSummary, MetricsRegistry,
    ResourceTracker, SceneDiff, SceneSummary, TraceBuffer, DEFAULT_TRACE_CAPACITY,
};

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
        "metrics" => {
            print_json(&observability_value("metrics"))?;
            Ok(())
        },
        "observability" | "obs" => {
            print_json(&observability_value("observability"))?;
            Ok(())
        },
        "scene-diff" | "scene_diff" => scene_diff_cli(&args[1..]),
        "trace-layer" | "trace_layer" => trace_layer::cli(&args[1..]),
        "serve" => serve(&args[1..]),
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
     metrics                     输出 additive metrics/resources/trace schema（CLI 未接线时为零）\n\
     observability               同上 + 集成点/能力声明（kind=observability）\n\
     scene-diff --left FILE --right FILE [--json]\n\
                                  比较两个 frame/scene summary JSON（默认文本）\n\
     trace-layer --input FILE     按 raw->filter->Term->Frame->framebuffer 逐层找第一处分歧\n\
                                  合成夹具 --fixture clean|images|filter-edge；--json\n\
     serve [--socket PATH --token T]  MCP/JSON-RPC stdio server；--socket 时增加 live 工具\n\
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

/// `tervo diag metrics|observability`: schema probe for the additive
/// observability fields.
///
/// The CLI process has no app frame loop, so the registry here is genuinely
/// empty. `initialized=false` / `source=cli` make that explicit instead of
/// pretending these are live app counters. A future live feed can reuse the
/// same document shape under `kind=live`.
fn observability_value(kind: &str) -> Value {
    let metrics = MetricsRegistry::new();
    let resources = AtomicResourceTracker::new();
    let trace = TraceBuffer::new(DEFAULT_TRACE_CAPACITY);
    let mut value = json!({
        "schema": "tervo.diag.v1",
        "kind": kind,
        "version": VERSION,
        "commit": COMMIT,
        "target": TARGET,
        "initialized": false,
        "source": "cli",
        "note": "no app frame loop in this process; counters are empty registry defaults until Renderer/Presenter feeds a shared registry",
        "metrics": metrics.snapshot(),
        "resources": resources.snapshot_json(),
        "trace": trace.snapshot_json(),
        "capabilities": {
            "atomic_metrics": true,
            "bounded_trace_ring": true,
            "scene_diff": true,
            "live_app_feed": false,
        },
    });
    if kind == "observability" {
        value["integration"] = integration_points();
    }
    value
}

/// Machine-readable integration seam for the next slice. Keeping this next to
/// the CLI output makes it obvious what is wired and what is still zero.
fn integration_points() -> Value {
    json!({
        "status": "not_wired",
        "reason": "observability P0 adds the standalone crate + CLI; the app render loop is intentionally untouched",
        "points": [
            {
                "path": "crates/app/src/render.rs",
                "symbol": "Renderer::draw_without_cursor",
                "feed": "MetricsRegistry::record_frame(FrameSample { cpu_time_us, damage_regions, damage_area, viewport_area, scene_revision })"
            },
            {
                "path": "crates/app/src/main.rs",
                "symbol": "redraw_pane_inner",
                "feed": "set_image_counts(frame.images.len(), placement_count); record_stale_frame() when frame.skipped"
            },
            {
                "path": "crates/app/src/main.rs",
                "symbol": "live_diag_snapshot",
                "feed": "merge registry.snapshot()/trace.snapshot_json()/resource snapshot into kind=live (additive fields only)"
            },
            {
                "path": "crates/app/src/render.rs",
                "symbol": "Renderer::image",
                "feed": "set_presenter_revision once the presenter has a monotonic revision"
            },
            {
                "path": "crates/graphics/src/lib.rs",
                "symbol": "GraphicsManager",
                "feed": "ResourceTracker: image resource count + width*height*4 bytes, placement count"
            },
            {
                "path": "GPU backend (future)",
                "symbol": "texture create/destroy",
                "feed": "record_texture_created/destroyed; set_gpu_textures(count, bytes) without per-frame GPU sync"
            }
        ]
    })
}

fn scene_diff_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    let left_path = opt(args, "--left").ok_or("scene-diff 需要 --left FILE")?;
    let right_path = opt(args, "--right").ok_or("scene-diff 需要 --right FILE")?;
    let left = read_scene_summary(&left_path)?;
    let right = read_scene_summary(&right_path)?;
    let diff = SceneDiff::between(&left, &right);
    if args.iter().any(|arg| arg == "--json") {
        print_json(&diff.to_json())?;
    } else {
        print!("{}", diff.to_text());
    }
    Ok(())
}

fn read_scene_summary(path: &str) -> Result<SceneSummary, Box<dyn Error>> {
    let body =
        std::fs::read_to_string(path).map_err(|e| format!("读取 scene 文件失败 {path}: {e}"))?;
    let value: Value =
        serde_json::from_str(&body).map_err(|e| format!("scene 文件不是 JSON {path}: {e}"))?;
    scene_summary_from_value(&value)
}

/// Accept both `tervo.diag.v1` frame JSON (`generation`, image `id`) and a
/// plain `SceneSummary` (`revision`, image `placement_id`). Unknown fields are
/// ignored so the schema stays additive.
fn scene_summary_from_value(value: &Value) -> Result<SceneSummary, Box<dyn Error>> {
    let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(0) as usize;
    let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(0) as usize;
    let revision = value
        .get("revision")
        .or_else(|| value.get("generation"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let text = value
        .get("text")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| row.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    let images = value
        .get("images")
        .and_then(Value::as_array)
        .map(|images| images.iter().filter_map(image_summary_from_value).collect())
        .unwrap_or_default();
    let damage = damage_summary_from_value(value.get("damage"));
    Ok(SceneSummary {
        revision,
        cols,
        rows,
        text,
        images,
        damage,
    })
}

fn image_summary_from_value(value: &Value) -> Option<ImageSummary> {
    let placement_id = value
        .get("placement_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_u64)?;
    Some(ImageSummary {
        placement_id,
        source_id: value.get("source_id").and_then(Value::as_u64).unwrap_or(0),
        row: value
            .get("row")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        col: value
            .get("col")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u16::MAX as u64) as u16,
        width: value
            .get("width")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        height: value
            .get("height")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        z_index: value
            .get("z_index")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    })
}

fn damage_summary_from_value(value: Option<&Value>) -> DamageSummary {
    let Some(value) = value else {
        return DamageSummary::default();
    };
    let full = value.get("full").and_then(Value::as_bool).unwrap_or(false);
    let mut rects = Vec::new();
    if let Some(array) = value.get("rects").and_then(Value::as_array) {
        for rect in array {
            let x = rect.get("x").and_then(Value::as_u64).unwrap_or(0);
            let y = rect.get("y").and_then(Value::as_u64).unwrap_or(0);
            let w = rect.get("w").and_then(Value::as_u64).unwrap_or(0);
            let h = rect.get("h").and_then(Value::as_u64).unwrap_or(0);
            let rect = DamageRect::new(
                x.min(u32::MAX as u64) as u32,
                y.min(u32::MAX as u64) as u32,
                w.min(u32::MAX as u64) as u32,
                h.min(u32::MAX as u64) as u32,
            );
            if !rect.is_empty() {
                rects.push(rect);
            }
        }
    }
    DamageSummary { full, rects }
}

/// 稳定 schema v1 的 frame 部分；图片只给 rect，不给 RGBA。
pub(crate) fn frame_json(frame: &Frame, kind: &str) -> Value {
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
    // 行段 damage 转成 cell rect，供 scene-diff/debug 使用；纯加法字段。
    let damage_rects: Vec<Value> = frame
        .damage
        .iter()
        .map(|damage| {
            json!({
                "x": damage.left,
                "y": damage.line,
                "w": damage.right.saturating_sub(damage.left),
                "h": 1,
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
        "damage": {
            "full": frame.full,
            "region_count": frame.damage.len(),
            "rects": damage_rects,
        },
        "images": images,
        "text": text,
    })
}

/// 极简 MCP/JSON-RPC stdio server。非插件；随 CLI 启动，stdin/stdout 通信。
///
/// 只暴露诊断读取；snapshot 会执行调用者给的 command（这是“启动一个 shell
/// 做诊断”，不是 Tervo 远程控制）。MCP client 应只在本地显式启动本进程。
fn serve(args: &[String]) -> Result<(), Box<dyn Error>> {
    use std::io::{BufRead, Write};
    let live_socket = opt(args, "--socket");
    let live_token = opt(args, "--token")
        .or_else(|| std::env::var("TERVO_DIAG_TOKEN").ok());
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
            "tools/list" => Some(json!({"tools": mcp_tools(live_socket.is_some())})),
            "tools/call" => Some(mcp_call(&req, live_socket.as_deref(), live_token.as_deref())),
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

fn mcp_tools(live: bool) -> Value {
    let mut tools = json!([
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
    ]);
    if live {
        if let Some(arr) = tools.as_array_mut() {
            arr.push(json!({
                "name": "tervo.live_capabilities",
                "description": "Read capabilities of the running Tervo live control socket (read-only).",
                "inputSchema": {"type": "object", "properties": {}}
            }));
            arr.push(json!({
                "name": "tervo.live_snapshot",
                "description": "Read all pane frames/cursor/images metadata from the running Tervo live control socket (read-only).",
                "inputSchema": {"type": "object", "properties": {}}
            }));
        }
    }
    tools
}

fn mcp_call(req: &Value, live_socket: Option<&str>, live_token: Option<&str>) -> Value {
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
        "tervo.live_capabilities" => match live_request(live_socket, live_token, "capabilities") {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into()),
        },
        "tervo.live_snapshot" => match live_request(live_socket, live_token, "snapshot") {
            Ok(v) => Ok(v),
            Err(e) => Err(e.into()),
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

fn live_request(
    socket: Option<&str>,
    token: Option<&str>,
    cmd: &str,
) -> Result<Value, String> {
    let socket = socket.ok_or("live control socket 未配置")?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("连接 live socket {socket} 失败: {e}"))?;
    use std::io::{BufRead, BufReader, Write};
    let req = serde_json::json!({"id": 1, "cmd": cmd, "token": token});
    writeln!(stream, "{req}").map_err(|e| format!("写 live socket 失败: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("读 live socket 失败: {e}"))?;
    let resp: Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("live socket 响应不是 JSON: {e}"))?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "live socket 拒绝: {}",
            resp.get("error").and_then(Value::as_str).unwrap_or("unknown")
        ));
    }
    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
}

fn mcp_error(code: i64, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

/// 只读 live control socket。仅当显式设置 `TERVO_DIAG_SOCKET` 时启动。
///
/// 协议：Unix stream 上的 JSON Lines。
///   {"id":1,"cmd":"capabilities"[, "token":"..."]}
///   {"id":2,"cmd":"snapshot"[, "token":"..."]}
/// 响应：{"id":1,"ok":true,"data":{...}} / {"id":1,"ok":false,"error":"..."}
///
/// `build_snapshot` 必须在 UI 线程执行（AppState/Slint 是线程局部）；这里用
/// `invoke_from_event_loop` 切回 UI，2s 超时，绝不在 socket 线程碰 AppState。
pub fn start_control_socket<F>(build_snapshot: F)
where
    F: Fn() -> Value + Send + Sync + 'static,
{
    let Some(path) = std::env::var_os("TERVO_DIAG_SOCKET") else { return };
    let path = std::path::PathBuf::from(path);
    let token = std::env::var("TERVO_DIAG_TOKEN").ok();
    let build = std::sync::Arc::new(build_snapshot);
    std::thread::Builder::new()
        .name("diag-socket".into())
        .spawn(move || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if path.exists() {
                    let _ = std::fs::remove_file(&path);
                }
                let listener = match std::os::unix::net::UnixListener::bind(&path) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("[diag] socket bind 失败 {}: {e}", path.display());
                        return;
                    },
                };
                if let Err(e) = std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(0o600),
                ) {
                    eprintln!("[diag] socket chmod 失败: {e}");
                }
                eprintln!(
                    "[diag] live control socket={} token={}",
                    path.display(),
                    if token.is_some() { "required" } else { "off" }
                );
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let build = build.clone();
                    let token = token.clone();
                    std::thread::Builder::new()
                        .name("diag-conn".into())
                        .spawn(move || handle_conn(stream, build.clone(), token.as_deref()))
                        .ok();
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (&path, &token, &build);
                eprintln!("[diag] control socket 仅支持 Unix");
            }
        })
        .ok();
}

fn handle_conn(
    mut stream: std::os::unix::net::UnixStream,
    build: std::sync::Arc<dyn Fn() -> Value + Send + Sync>,
    token: Option<&str>,
) {
    use std::io::{BufRead, BufReader, Write};
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let reader = BufReader::new(stream.try_clone().expect("clone diag stream"));
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"ok":false,"error":format!("bad json: {e}")})).unwrap());
                continue;
            },
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let need = token.is_some();
        if need && req.get("token").and_then(Value::as_str) != token {
            let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":false,"error":"unauthorized"})).unwrap());
            continue;
        }
        match req.get("cmd").and_then(Value::as_str) {
            Some("capabilities") => {
                let data = json!({
                    "schema":"tervo.diag.v1",
                    "live": true,
                    "read_only": true,
                    "commands":["capabilities","snapshot"],
                });
                let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":true,"data":data})).unwrap());
            },
            Some("snapshot") => {
                let (tx, rx) = std::sync::mpsc::channel();
                let build = build.clone();
                let sent = slint::invoke_from_event_loop(move || {
                    let _ = tx.send(build());
                });
                if sent.is_err() {
                    let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":false,"error":"event loop closed"})).unwrap());
                    continue;
                }
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(data) => {
                        let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":true,"data":data})).unwrap());
                    },
                    Err(_) => {
                        let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":false,"error":"snapshot timeout"})).unwrap());
                    },
                }
            },
            _ => {
                let _ = writeln!(stream, "{}", serde_json::to_string(&json!({"id":id,"ok":false,"error":"unknown cmd"})).unwrap());
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_exposes_read_only_diag_tools() {
        let tools = mcp_tools(false);
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
        assert_eq!(value["damage"]["full"], false);
        assert_eq!(value["damage"]["region_count"], 0);
        assert!(value.get("data").is_none());
    }

    #[test]
    fn scene_diff_parses_frame_json_and_reports_counts() {
        let left = json!({
            "schema": "tervo.diag.v1",
            "kind": "snapshot",
            "cols": 6,
            "rows": 2,
            "generation": 5,
            "text": ["abc", "def"],
            "images": [
                {"id": 1, "source_id": 11, "row": 0, "col": 0, "width": 2, "height": 1}
            ],
            "damage": {
                "full": false,
                "region_count": 1,
                "rects": [{"x": 0, "y": 0, "w": 2, "h": 1}]
            }
        });
        let right = json!({
            "schema": "tervo.diag.v1",
            "kind": "snapshot",
            "cols": 6,
            "rows": 2,
            "generation": 6,
            "text": ["abd", "def"],
            "images": [
                {"id": 1, "source_id": 11, "row": 1, "col": 2, "width": 2, "height": 1}
            ],
            "damage": {
                "full": false,
                "region_count": 1,
                "rects": [{"x": 3, "y": 0, "w": 2, "h": 1}]
            }
        });
        let left = scene_summary_from_value(&left).unwrap();
        let right = scene_summary_from_value(&right).unwrap();
        assert_eq!(left.revision, 5);
        assert_eq!(left.images[0].placement_id, 1);
        assert_eq!(left.damage.region_count(), 1);

        let diff = SceneDiff::between(&left, &right);
        assert_eq!(diff.text.changed_rows, 1);
        assert_eq!(diff.text.changed_cells, 1);
        assert_eq!(diff.images.moved.len(), 1);
        assert_eq!(diff.images.added.len(), 0);
        assert_eq!(diff.damage.regions_added.len(), 1);
        assert_eq!(diff.damage.regions_removed.len(), 1);
        let value = diff.to_json();
        assert_eq!(value["schema"], "tervo.diag.v1");
        assert_eq!(value["kind"], "scene_diff");
        assert_eq!(value["images"]["counts"]["moved"], 1);
    }

    #[test]
    fn observability_document_is_additive_and_marks_unfed_collector() {
        let value = observability_value("observability");
        assert_eq!(value["schema"], "tervo.diag.v1");
        assert_eq!(value["kind"], "observability");
        assert_eq!(value["initialized"], false);
        assert_eq!(value["source"], "cli");
        assert_eq!(value["metrics"]["frame_count"], 0);
        assert_eq!(value["metrics"]["damage"]["regions_last"], 0);
        assert_eq!(value["trace"]["capacity"], DEFAULT_TRACE_CAPACITY);
        assert_eq!(value["resources"]["gpu_textures"]["count"], 0);
        assert!(value["integration"]["points"].as_array().unwrap().len() >= 4);

        let metrics = observability_value("metrics");
        assert_eq!(metrics["kind"], "metrics");
        assert!(metrics.get("integration").is_none());
    }
}
