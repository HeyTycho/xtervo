// SPDX-License-Identifier: GPL-3.0-only
//! 内置 harness-neutral 终端诊断 CLI（`tervo diag`）。
//!
//! 不创建窗口、不初始化 Slint；输出 `tervo.diag.v1` JSON，任何 harness 都可以
//! 在 shell 里调用。提供 env / snapshot / replay，以及本地 MCP stdio server
//! （`tervo diag serve`）。
//!
//! 安全：不输出环境变量值、API key、Vault 内容或图片 RGBA；只给版本、路径、
//! frame 文本、cursor/modes 和图片 rect。MCP 的 doctor/issue/metrics 工具只读，
//! 不启动进程、不写文件；snapshot 仍会执行调用者显式给出的诊断命令。

mod mcp_ext;
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
/// One JSON-RPC line can carry a diagnostic case plus frame text. 1 MiB is
/// generous for that use and still prevents an accidentally unbounded request.
const MCP_MAX_REQUEST_BYTES: usize = 1024 * 1024;

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
       resources: tervo://version；tools: diag_env/diag_snapshot/diag_replay/\n\
       doctor_check/issue_draft/metrics_read（后 3 个只读、无 shell）\n\
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
    let live_socket = opt(args, "--socket");
    let live_token = opt(args, "--token")
        .or_else(|| std::env::var("TERVO_DIAG_TOKEN").ok());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_reader_writer(
        stdin.lock(),
        stdout.lock(),
        live_socket.as_deref(),
        live_token.as_deref(),
    )
}

/// Testable transport loop: one JSON-RPC object per line, responses on writer.
///
/// Notifications never produce a line. Malformed JSON is answered with the
/// standard -32700 error instead of being silently dropped, so an MCP client
/// can tell that its request was not understood.
fn serve_reader_writer<R, W>(
    reader: R,
    mut writer: W,
    live_socket: Option<&str>,
    live_token: Option<&str>,
) -> Result<(), Box<dyn Error>>
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = if line.len() > MCP_MAX_REQUEST_BYTES {
            Some(mcp_protocol_error(
                Value::Null,
                -32600,
                "request too large",
            ))
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(request) => mcp_handle_request(&request, live_socket, live_token),
                Err(error) => {
                    eprintln!("[diag] MCP 坏 JSON: {error}");
                    Some(mcp_protocol_error(Value::Null, -32700, "parse error"))
                },
            }
        };
        let Some(response) = response else { continue };
        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
        writer.flush()?;
    }
    Ok(())
}

fn mcp_handle_request(
    request: &Value,
    live_socket: Option<&str>,
    live_token: Option<&str>,
) -> Option<Value> {
    let object = match request.as_object() {
        Some(object) => object,
        None => {
            return Some(mcp_protocol_error(
                Value::Null,
                -32600,
                "invalid request: expected an object",
            ))
        },
    };
    if let Some(version) = object.get("jsonrpc").and_then(Value::as_str) {
        if version != "2.0" {
            return Some(mcp_protocol_error(
                object.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                "invalid jsonrpc version",
            ));
        }
    }
    let Some(id) = object.get("id").cloned() else {
        // JSON-RPC notification: never write a response.
        return None;
    };
    if !(id.is_null() || id.is_string() || id.is_number()) {
        return Some(mcp_protocol_error(
            Value::Null,
            -32600,
            "invalid request id",
        ));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(mcp_protocol_error(id, -32600, "missing method"));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    if !params.is_null() && !params.is_object() {
        return Some(mcp_protocol_error(
            id,
            -32602,
            "params must be an object",
        ));
    }
    match method {
        "initialize" => Some(mcp_result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"subscribe": false, "listChanged": false}
                },
                "serverInfo": {"name": "tervo-diag", "version": VERSION},
            }),
        )),
        "notifications/initialized" => None,
        "ping" => Some(mcp_result(id, json!({}))),
        "tools/list" => {
            Some(mcp_result(id, json!({"tools": mcp_tools(live_socket.is_some())})))
        },
        "tools/call" => {
            let result = mcp_call(request, live_socket, live_token);
            Some(match result.get("error") {
                Some(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                None => mcp_result(id, result),
            })
        },
        "resources/list" => Some(mcp_result(id, mcp_ext::resources_list())),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str);
            match uri {
                Some(uri) => match mcp_ext::read_resource(uri) {
                    Ok(result) => Some(mcp_result(id, result)),
                    Err(_) => Some(mcp_protocol_error(id, -32002, "resource not found")),
                },
                None => Some(mcp_protocol_error(id, -32602, "uri is required")),
            }
        },
        _ => Some(mcp_protocol_error(id, -32601, "method not found")),
    }
}

fn mcp_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_protocol_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
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
        },
        {
            "name": "tervo.doctor_check",
            "description": "Run deterministic, offline Tervo doctor checks and return a tervo.diagnostic_case.v1 JSON document. Read-only; no shell, no writes. Inputs may be inline JSON objects or paths under $HOME or /tmp.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "environment": {
                        "type": ["object", "string"],
                        "description": "`tervo diag env --json` object or a path to it"
                    },
                    "frame": {
                        "type": ["object", "string"],
                        "description": "`tervo diag snapshot --json` object or a path to it"
                    },
                    "metrics": {
                        "type": ["object", "string"],
                        "description": "`tervo diag metrics` JSON object or a path to it"
                    },
                    "symptom": {"type": "string"},
                    "details": {"type": "string"},
                    "repro_steps": {"type": "array", "items": {"type": "string"}},
                    "expected": {"type": "string"},
                    "actual": {"type": "string"}
                },
                "additionalProperties": false
            }
        },
        {
            "name": "tervo.issue_draft",
            "description": "Render the fixed doctor issue template (markdown) from a complete tervo.diagnostic_case.v1 object. Read-only; no model, no network.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "case": {
                        "type": "object",
                        "description": "A complete tervo.diagnostic_case.v1 document, for example the output of tervo.doctor_check"
                    }
                },
                "required": ["case"],
                "additionalProperties": false
            }
        },
        {
            "name": "tervo.metrics_read",
            "description": "Bounded read-only summary of Tervo's log directory: file/byte/line counts and recent error-like lines. Only tervo-*.log files under $HOME or /tmp are considered; raw lines and secrets are never returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "log_dir": {
                        "type": "string",
                        "description": "Optional Tervo log directory under $HOME or /tmp; defaults to TERVO_LOG_DIR/XDG state path"
                    }
                },
                "additionalProperties": false
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
    if !params.is_object() {
        return mcp_error(-32602, "params must be an object");
    }
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return mcp_error(-32602, "arguments must be an object");
    }
    let result: Result<Value, Box<dyn Error>> = match name {
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
        "tervo.doctor_check" => mcp_ext::doctor_check(&args).map_err(|error| error.into()),
        "tervo.issue_draft" => mcp_ext::issue_draft(&args).map_err(|error| error.into()),
        "tervo.metrics_read" => mcp_ext::metrics_read(&args).map_err(|error| error.into()),
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
    use std::io::Cursor;
    use std::path::{Path, PathBuf};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "tervo-mcp-test-{}-{}-{stamp}",
                label,
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test dir");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        fn write(&self, name: &str, body: &str) -> PathBuf {
            let path = self.join(name);
            std::fs::write(&path, body).expect("write test file");
            path
        }

        fn as_str(&self) -> &str {
            self.path.to_str().expect("UTF-8 temp path")
        }

        fn snapshot(&self) -> Vec<(String, u64)> {
            let mut entries = Vec::new();
            snapshot_dir(&self.path, &self.path, &mut entries);
            entries.sort();
            entries
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn snapshot_dir(root: &Path, path: &Path, entries: &mut Vec<(String, u64)>) {
        let Ok(read_dir) = std::fs::read_dir(path) else {
            return;
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let relative = path
                .strip_prefix(root)
                .map(|prefix| prefix.join(&name))
                .unwrap_or_else(|_| PathBuf::from(&name))
                .to_string_lossy()
                .to_string();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                entries.push((format!("{relative}/"), 0));
                snapshot_dir(root, &entry.path(), entries);
            } else {
                entries.push((relative, metadata.len()));
            }
        }
    }

    fn mcp_request(id: u64, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn mcp_run(lines: &[Value]) -> Vec<Value> {
        let input = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = Vec::new();
        serve_reader_writer(Cursor::new(input.into_bytes()), &mut output, None, None)
            .expect("serve stdio");
        String::from_utf8(output)
            .expect("UTF-8 MCP output")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON-RPC response"))
            .collect()
    }

    fn mcp_call_tool(name: &str, arguments: Value) -> Value {
        let responses = mcp_run(&[mcp_request(
            1,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        )]);
        assert_eq!(responses.len(), 1);
        responses.into_iter().next().unwrap()
    }

    fn mcp_tool_value(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text content");
        serde_json::from_str(text).expect("tool JSON text")
    }

    fn mcp_is_error(response: &Value) -> bool {
        response["result"]["isError"].as_bool() == Some(true)
    }

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
        assert!(names.contains(&"tervo.doctor_check"));
        assert!(names.contains(&"tervo.issue_draft"));
        assert!(names.contains(&"tervo.metrics_read"));
        for tool in tools.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
        }

        let live_tools = mcp_tools(true);
        let live_names: Vec<_> = live_tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(live_names.contains(&"tervo.live_snapshot"));
    }

    #[test]
    fn mcp_initialize_advertises_tools_and_resources() {
        let responses = mcp_run(&[mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "tervo-test", "version": "0"}
            }),
        )]);
        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["capabilities"]["resources"]["subscribe"], false);
        assert_eq!(
            result["capabilities"]["resources"]["listChanged"],
            false
        );
        assert_eq!(result["serverInfo"]["name"], "tervo-diag");
        assert_eq!(result["serverInfo"]["version"], VERSION);
    }

    #[test]
    fn mcp_ping_and_initialized_notification() {
        let responses = mcp_run(&[
            mcp_request(1, "initialize", json!({})),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {}}),
            mcp_request(2, "ping", json!({})),
        ]);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[1]["result"], json!({}));
    }

    #[test]
    fn mcp_unknown_method_and_tool_return_protocol_errors() {
        let responses = mcp_run(&[
            mcp_request(1, "unknown/method", json!({})),
            mcp_request(
                2,
                "tools/call",
                json!({"name": "tervo.unknown_tool", "arguments": {}}),
            ),
        ]);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32601);
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert!(responses[1].get("result").is_none());
    }

    #[test]
    fn mcp_parse_error_returns_32700() {
        let mut output = Vec::new();
        serve_reader_writer(
            Cursor::new(b"this is not json\n".to_vec()),
            &mut output,
            None,
            None,
        )
        .expect("serve malformed line");
        let response: Value = serde_json::from_slice(&output).expect("parse error response");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], -32700);
    }

    #[test]
    fn mcp_doctor_check_returns_diagnostic_case() {
        let response = mcp_call_tool("tervo.doctor_check", json!({}));
        assert!(!mcp_is_error(&response));
        let case = mcp_tool_value(&response);
        assert_eq!(case["schema"], "tervo.diagnostic_case.v1");
        assert!(case["case_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("case-"));
        assert!(!case["probes"].as_array().unwrap().is_empty());
        assert!(case["issue_draft"]["markdown"]
            .as_str()
            .unwrap_or_default()
            .contains("## 环境"));
    }

    #[test]
    fn mcp_doctor_check_accepts_inline_frame_and_metrics() {
        let frame = json!({
            "schema": "tervo.diag.v1",
            "kind": "snapshot",
            "cols": 80,
            "rows": 2,
            "generation": 7,
            "cursor": {"row": 0, "col": 0, "visible": true, "shape": "block"},
            "damage": {"full": false, "region_count": 0, "rects": []},
            "images": [],
            "text": ["hello", "world"]
        });
        let metrics = json!({
            "schema": "tervo.diag.v1",
            "kind": "metrics",
            "initialized": false,
            "source": "cli",
            "metrics": {
                "frame_count": 0,
                "frame_time": {"p95_us": null, "samples": 0, "p95_overflow": false},
                "damage": {"area_ratio_last_ppm": 0, "area_ratio_max_ppm": 0},
                "stale_frames": 0,
                "textures": {"live_bytes_estimate": 0},
                "images": {"placements": 0}
            }
        });
        let response = mcp_call_tool(
            "tervo.doctor_check",
            json!({
                "frame": frame,
                "metrics": metrics,
                "symptom": "scroll residue",
                "repro_steps": ["open tervo", "scroll"]
            }),
        );
        assert!(!mcp_is_error(&response));
        let case = mcp_tool_value(&response);
        assert_eq!(case["symptom"]["summary"], "scroll residue");
        assert_eq!(case["symptom"]["repro_steps"][0], "open tervo");
        assert!(case["probes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|probe| probe["id"] == "probe.frame"));
    }

    #[test]
    fn mcp_doctor_check_accepts_bounded_path_inputs() {
        let dir = TestDir::new("doctor-paths");
        let config = dir.write("config.toml", "theme = \"dark\"\n");
        let profile_path = dir.join("profiles.toml");
        let frame_path = dir.write(
            "frame.json",
            &json!({
                "schema": "tervo.diag.v1",
                "kind": "snapshot",
                "cols": 20,
                "rows": 1,
                "generation": 1,
                "cursor": {"row": 0, "col": 0, "visible": true, "shape": "block"},
                "damage": {"full": false, "region_count": 0, "rects": []},
                "images": [],
                "text": ["hi"]
            })
            .to_string(),
        );
        let env_path = dir.write(
            "env.json",
            &json!({
                "schema": "tervo.diag.v1",
                "kind": "env",
                "version": "0.3.1",
                "commit": "deadbeef",
                "dirty": false,
                "built": "2026-11-12T00:00:00Z",
                "target": "x86_64-unknown-linux-gnu",
                "dri": false,
                "config": config.to_string_lossy(),
                "profiles": profile_path.to_string_lossy(),
                "log": null
            })
            .to_string(),
        );
        let response = mcp_call_tool(
            "tervo.doctor_check",
            json!({
                "environment": env_path.to_string_lossy(),
                "frame": frame_path.to_string_lossy()
            }),
        );
        assert!(!mcp_is_error(&response));
        let case = mcp_tool_value(&response);
        assert_eq!(case["environment"]["version"], "0.3.1");
        assert_eq!(
            case["environment"]["config_path"],
            config.to_string_lossy().as_ref()
        );
        assert!(case["probes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|probe| probe["id"] == "probe.config"));
    }

    #[test]
    fn mcp_doctor_check_rejects_paths_outside_home_and_tmp() {
        for arguments in [
            json!({"environment": "/etc/passwd"}),
            json!({"environment": {
                "schema": "tervo.diag.v1",
                "kind": "env",
                "config": "/etc/passwd"
            }}),
        ] {
            let response = mcp_call_tool("tervo.doctor_check", arguments);
            assert!(mcp_is_error(&response));
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default();
            assert!(
                text.contains("outside") || text.contains("allowed"),
                "{text}"
            );
        }
    }

    #[test]
    fn mcp_issue_draft_renders_markdown_from_case() {
        let check = mcp_call_tool("tervo.doctor_check", json!({"symptom": "flicker"}));
        assert!(!mcp_is_error(&check));
        let case = mcp_tool_value(&check);
        let case_id = case["case_id"].clone();
        let response = mcp_call_tool("tervo.issue_draft", json!({"case": case}));
        assert!(!mcp_is_error(&response));
        let draft = mcp_tool_value(&response);
        let markdown = draft["markdown"].as_str().unwrap_or_default();
        assert!(markdown.contains("## 现象"), "{markdown}");
        assert!(markdown.contains("## 环境"), "{markdown}");
        assert_eq!(draft["case_id"], case_id);
    }

    #[test]
    fn mcp_metrics_read_is_bounded_and_hides_raw_lines() {
        let dir = TestDir::new("metrics");
        let body = "boot\nframe error: boom\nok\n";
        dir.write("tervo-1.log", body);
        dir.write("notes.log", "API_KEY=super-secret\n");
        dir.write("tervo-notlog.txt", "fatal secret\n");

        let response = mcp_call_tool("tervo.metrics_read", json!({"log_dir": dir.as_str()}));
        assert!(!mcp_is_error(&response));
        let summary = mcp_tool_value(&response);
        assert_eq!(summary["exists"], true);
        assert_eq!(summary["summary"]["matched_files"], 1);
        assert_eq!(summary["summary"]["stats_files"], 1);
        assert_eq!(summary["summary"]["lines"], 3);
        assert_eq!(summary["summary"]["recent_errors"], 1);
        assert_eq!(summary["summary"]["bytes"], body.len() as u64);
        assert_eq!(summary["files"][0]["name"], "tervo-1.log");

        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(!text.contains("super-secret"), "{text}");
        assert!(!text.contains("API_KEY"), "{text}");
        assert!(!text.contains("frame error: boom"), "{text}");
    }

    #[test]
    fn mcp_metrics_read_rejects_outside_paths() {
        for path in ["/etc", "relative/logs"] {
            let response = mcp_call_tool("tervo.metrics_read", json!({"log_dir": path}));
            assert!(mcp_is_error(&response), "{path} should be rejected");
        }
    }

    #[cfg(unix)]
    #[test]
    fn mcp_metrics_read_rejects_symlink_escape() {
        let dir = TestDir::new("symlink");
        let link = dir.join("escape");
        std::os::unix::fs::symlink("/etc", &link).expect("create symlink");
        let response = mcp_call_tool(
            "tervo.metrics_read",
            json!({"log_dir": link.to_string_lossy()}),
        );
        assert!(mcp_is_error(&response));
    }

    #[test]
    fn mcp_resources_expose_version_document() {
        let responses = mcp_run(&[
            mcp_request(1, "resources/list", json!({})),
            mcp_request(2, "resources/read", json!({"uri": "tervo://version"})),
            mcp_request(3, "resources/read", json!({"uri": "tervo://missing"})),
        ]);
        assert_eq!(
            responses[0]["result"]["resources"][0]["uri"],
            "tervo://version"
        );
        let content = &responses[1]["result"]["contents"][0];
        assert_eq!(content["uri"], "tervo://version");
        let version: Value =
            serde_json::from_str(content["text"].as_str().unwrap_or_default()).unwrap();
        assert_eq!(version["version"], VERSION);
        assert_eq!(version["schema"], "tervo.diag.v1");
        assert_eq!(responses[2]["error"]["code"], -32002);
    }

    #[test]
    fn mcp_read_only_tools_do_not_write_files() {
        let dir = TestDir::new("readonly");
        let config = dir.write("config.toml", "theme = \"dark\"\n");
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).expect("create logs dir");
        std::fs::write(logs.join("tervo-1.log"), "one\ntwo\n").expect("write log");
        let before = dir.snapshot();

        let environment = json!({
            "schema": "tervo.diag.v1",
            "kind": "env",
            "version": VERSION,
            "commit": COMMIT,
            "dirty": DIRTY == "1",
            "built": BUILT,
            "target": TARGET,
            "dri": false,
            "config": config.to_string_lossy(),
            "log_dir": logs.to_string_lossy()
        });
        let check = mcp_call_tool("tervo.doctor_check", json!({"environment": environment}));
        assert!(!mcp_is_error(&check));
        let case = mcp_tool_value(&check);
        let draft = mcp_call_tool("tervo.issue_draft", json!({"case": case}));
        assert!(!mcp_is_error(&draft));
        let metrics = mcp_call_tool(
            "tervo.metrics_read",
            json!({"log_dir": logs.to_string_lossy()}),
        );
        assert!(!mcp_is_error(&metrics));

        assert_eq!(before, dir.snapshot());
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
