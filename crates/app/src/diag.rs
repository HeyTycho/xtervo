// SPDX-License-Identifier: GPL-3.0-only
//! 内置 harness-neutral 终端诊断 CLI（`xtervo diag`）。
//!
//! 不创建窗口、不初始化 Slint；输出 `xtervo.diag.v1` JSON，任何 harness 都可以
//! 在 shell 里调用。提供 env / snapshot / replay，以及本地 MCP stdio server
//! （`xtervo diag serve`）。
//!
//! 安全：不输出环境变量值、API key、Vault 内容或图片 RGBA；只给版本、路径、
//! frame 文本、cursor/modes 和图片 rect。MCP 的 doctor/issue/metrics 工具只读，
//! 不启动进程、不写文件；snapshot 仍会执行调用者显式给出的诊断命令。

mod mcp_ext;
mod trace_layer;

use std::error::Error;
use std::time::Duration;

use serde_json::{json, Value};
use xtervo_core::{spawn, Frame, PaneConfig};
use xtervo_diagnostics::{
    AtomicResourceTracker, DamageRect, DamageSummary, ImageSummary, MetricsRegistry,
    ResourceTracker, SceneDiff, SceneSummary, TraceBuffer, DEFAULT_TRACE_CAPACITY,
};

use crate::config::Config;
use crate::logging;
use crate::mcp;
use crate::profile;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: &str = env!("XTERVO_GIT_COMMIT");
const DIRTY: &str = env!("XTERVO_GIT_DIRTY");
const BUILT: &str = env!("XTERVO_BUILD_TIME");
const TARGET: &str = env!("XTERVO_BUILD_TARGET");
/// One JSON-RPC line can carry a diagnostic case plus frame text. 1 MiB is
/// generous for that use and still prevents an accidentally unbounded request.
const MCP_MAX_REQUEST_BYTES: usize = 1024 * 1024;

pub fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    let sub = args.first().map(String::as_str).unwrap_or("env");
    match sub {
        "env" | "--json" => {
            print_json(&env_json())?;
            Ok(())
        }
        "snapshot" => snapshot(&args[1..]),
        "replay" => replay(&args[1..]),
        "trace" => trace(&args[1..]),
        "metrics" => {
            print_json(&observability_value("metrics"))?;
            Ok(())
        }
        "observability" | "obs" => {
            print_json(&observability_value("observability"))?;
            Ok(())
        }
        "scene-diff" | "scene_diff" => scene_diff_cli(&args[1..]),
        "trace-layer" | "trace_layer" => trace_layer::cli(&args[1..]),
        "doctor" | "doctor-check" => doctor_cli(&args[1..]),
        "serve" => serve(&args[1..]),
        "help" | "-h" | "--help" => {
            println!("{}", help_text());
            Ok(())
        }
        other => Err(format!("未知 diag 子命令: {other}（用 xtervo diag help）").into()),
    }
}

fn help_text() -> &'static str {
    "用法: xtervo diag [env|snapshot|replay] [选项]\n\
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
     doctor [--issue] [--frame FILE] [--metrics FILE]\n\
                                  Doctor 确定性诊断；--issue 输出八节 issue markdown\n\
     serve [--socket PATH] [--token T]  MCP/JSON-RPC stdio server\n\
       无参数时自动读取 config/state：$XDG_RUNTIME_DIR/xtervo/mcp.sock\n\
       + state token；--socket/--token 与环境变量仍是测试/CI override\n\
       live tools 按运行中 xtervo 的 MCP 权限分组过滤；app 端二次校验\n\
       离线 tools: diag_env/diag_snapshot/diag_replay/\n\
       doctor_check/issue_draft/metrics_read（后 3 个只读、无 shell）\n\
     \n\
     输出 schema=xtervo.diag.v1；不含 API key/环境变量值。"
}

fn opt(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
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
        "schema": "xtervo.diag.v1",
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
        "platform_hint": crate::platform::SessionFacts::from_env().hint().label(),
        "xwayland": crate::platform::SessionFacts::from_env().xwayland_likely(),
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
        seq,
        out_path,
        duration,
        final_value.as_ref().and_then(|v| v.get("generation"))
    );
    Ok(())
}

/// `xtervo diag metrics|observability`: schema probe for the additive
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
        "schema": "xtervo.diag.v1",
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

/// Accept both `xtervo.diag.v1` frame JSON (`generation`, image `id`) and a
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
        "schema": "xtervo.diag.v1",
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

/// 设置页 Help → Run Doctor 使用：只跑确定性 Doctor，不调用模型。
pub(crate) fn doctor_markdown() -> Result<String, String> {
    let case = mcp_ext::doctor_check(&json!({}))?;
    case.get("issue_draft")
        .and_then(|draft| draft.get("markdown"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "doctor case 缺少 issue_draft.markdown".to_string())
}

/// `xtervo diag doctor`：把已有 Doctor 确定性核心暴露成 app CLI 入口。
/// 不联系模型、不读 scrollback；只使用显式传入的 frame/metrics 文件和本进程环境。
fn doctor_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    let mut input = json!({});
    if let Some(path) = opt(args, "--frame") {
        input["frame"] = json!(path);
    }
    if let Some(path) = opt(args, "--metrics") {
        input["metrics"] = json!(path);
    }
    let case = mcp_ext::doctor_check(&input).map_err(|e| format!("doctor: {e}"))?;
    if args.iter().any(|a| a == "--issue") {
        let markdown = case
            .get("issue_draft")
            .and_then(|draft| draft.get("markdown"))
            .and_then(Value::as_str)
            .ok_or("doctor case 缺少 issue_draft.markdown")?;
        println!("{markdown}");
    } else {
        println!("{}", serde_json::to_string_pretty(&case)?);
    }
    Ok(())
}

/// MCP stdio bridge 的 live socket 目标。无参数启动时自动发现：
/// socket 默认路径来自配置/环境，token 默认读 state 文件（0600）。
/// `--socket` / `--token` 与同名环境变量是测试/CI 的 override。
#[derive(Debug, Default)]
struct LiveTarget {
    socket: Option<String>,
    token: Option<String>,
    token_file: Option<std::path::PathBuf>,
    last_probe_error: Option<String>,
}

impl LiveTarget {
    fn resolve_token(&mut self) {
        if self.token.is_some() {
            return;
        }
        let Some(path) = self.token_file.as_deref() else {
            return;
        };
        if let Some(token) = crate::mcp::read_token_file(path) {
            self.token = Some(token);
        }
    }

    fn exchange(&mut self, command: &str, args: &Value, probe: bool) -> Result<Value, String> {
        self.resolve_token();
        let first = live_exchange(
            self.socket.as_deref(),
            self.token.as_deref(),
            command,
            args,
            probe,
        );
        if self.token_file.is_some()
            && first
                .as_ref()
                .err()
                .is_some_and(|error| error.contains("unauthorized"))
        {
            // xtervo 可能刚刚生成/轮换 state token：清缓存重读一次。
            self.token = None;
            self.resolve_token();
            return live_exchange(
                self.socket.as_deref(),
                self.token.as_deref(),
                command,
                args,
                probe,
            );
        }
        first
    }

    /// 向 app 询问当前允许的命令；失败时返回 None（live 工具全部隐藏）。
    fn commands(&mut self) -> Option<Vec<String>> {
        if self.socket.is_none() {
            return None;
        }
        let probe = json!({});
        match self.exchange("capabilities", &probe, true) {
            Ok(value) => {
                self.last_probe_error = None;
                commands_from_capabilities(&value)
            }
            Err(error) => {
                if self.last_probe_error.as_deref() != Some(error.as_str()) {
                    eprintln!("[diag] live capability probe 失败: {error}");
                    self.last_probe_error = Some(error);
                }
                None
            }
        }
    }
}

fn commands_from_capabilities(value: &Value) -> Option<Vec<String>> {
    let commands = value
        .get("commands")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    Some(commands)
}

/// 极简 MCP/JSON-RPC stdio server。非插件；随 CLI 启动，stdin/stdout 通信。
///
/// 只暴露诊断读取；snapshot 会执行调用者给的 command（这是“启动一个 shell
/// 做诊断”，不是 xtervo 远程控制）。MCP client 应只在本地显式启动本进程。
/// live 工具列表以运行中 app 的 `capabilities.commands` 为准，而不是本进程 env。
fn serve(args: &[String]) -> Result<(), Box<dyn Error>> {
    let explicit_socket = opt(args, "--socket")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var(crate::mcp::SOCKET_ENV)
                .ok()
                .filter(|v| !v.is_empty())
        });
    let explicit_token = opt(args, "--token")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var(crate::mcp::TOKEN_ENV)
                .ok()
                .filter(|v| !v.is_empty())
        });
    let config_enabled = crate::mcp::config_enabled_from_disk();
    // 无参数时先指向默认路径；app 稍后启动也能在下次 tools/list 发现。
    let socket = explicit_socket.or_else(|| {
        config_enabled.then(|| crate::mcp::default_socket_path().display().to_string())
    });
    let token_file = explicit_token.is_none().then(crate::mcp::token_file_path);
    eprintln!(
        "[diag] MCP live target socket={} token={} config_enabled={config_enabled}",
        socket.as_deref().unwrap_or("offline"),
        if explicit_token.is_some() {
            "cli/env"
        } else if token_file.is_some() {
            "state-file"
        } else {
            "none"
        }
    );
    let live = LiveTarget {
        socket,
        token: explicit_token,
        token_file,
        ..LiveTarget::default()
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    serve_reader_writer(stdin.lock(), stdout.lock(), live)
}

/// Testable transport loop: one JSON-RPC object per line, responses on writer.
///
/// Notifications never produce a line. Malformed JSON is answered with the
/// standard -32700 error instead of being silently dropped, so an MCP client
/// can tell that its request was not understood.
fn serve_reader_writer<R, W>(
    reader: R,
    mut writer: W,
    mut live: LiveTarget,
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
            Some(mcp_protocol_error(Value::Null, -32600, "request too large"))
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(request) => mcp_handle_request(&request, &mut live),
                Err(error) => {
                    eprintln!("[diag] MCP 坏 JSON: {error}");
                    Some(mcp_protocol_error(Value::Null, -32700, "parse error"))
                }
            }
        };
        let Some(response) = response else { continue };
        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
        writer.flush()?;
    }
    Ok(())
}

fn mcp_handle_request(request: &Value, live: &mut LiveTarget) -> Option<Value> {
    let object = match request.as_object() {
        Some(object) => object,
        None => {
            return Some(mcp_protocol_error(
                Value::Null,
                -32600,
                "invalid request: expected an object",
            ))
        }
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
        return Some(mcp_protocol_error(id, -32602, "params must be an object"));
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
                "serverInfo": {"name": "xtervo-diag", "version": VERSION},
            }),
        )),
        "notifications/initialized" => None,
        "ping" => Some(mcp_result(id, json!({}))),
        "tools/list" => {
            // live 工具必须跟运行中 app 的权限一致；server 自己的 env 不再
            // 决定列表（app 端 tools/call 仍会二次校验）。
            let commands = live.commands().unwrap_or_default();
            Some(mcp_result(id, json!({"tools": mcp_tools(&commands)})))
        }
        "tools/call" => {
            let result = mcp_call(request, live);
            Some(match result.get("error") {
                Some(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                None => mcp_result(id, result),
            })
        }
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
        }
        _ => Some(mcp_protocol_error(id, -32601, "method not found")),
    }
}

fn mcp_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_protocol_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn mcp_tools(commands: &[String]) -> Value {
    let has = |command: &str| commands.iter().any(|allowed| allowed == command);
    let mut tools = json!([
        {
            "name": "xtervo.diag_env",
            "description": "xtervo version/display/config/log paths; no env values or secrets.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "xtervo.diag_snapshot",
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
            "name": "xtervo.diag_replay",
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
            "name": "xtervo.doctor_check",
            "description": "Run deterministic, offline xtervo doctor checks and return a xtervo.diagnostic_case.v1 JSON document. Read-only; no shell, no writes. Inputs may be inline JSON objects or paths under $HOME or /tmp.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "environment": {
                        "type": ["object", "string"],
                        "description": "`xtervo diag env --json` object or a path to it"
                    },
                    "frame": {
                        "type": ["object", "string"],
                        "description": "`xtervo diag snapshot --json` object or a path to it"
                    },
                    "metrics": {
                        "type": ["object", "string"],
                        "description": "`xtervo diag metrics` JSON object or a path to it"
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
            "name": "xtervo.issue_draft",
            "description": "Render the fixed doctor issue template (markdown) from a complete xtervo.diagnostic_case.v1 object. Read-only; no model, no network.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "case": {
                        "type": "object",
                        "description": "A complete xtervo.diagnostic_case.v1 document, for example the output of xtervo.doctor_check"
                    }
                },
                "required": ["case"],
                "additionalProperties": false
            }
        },
        {
            "name": "xtervo.metrics_read",
            "description": "Bounded read-only summary of xtervo's log directory: file/byte/line counts and recent error-like lines. Only xtervo-*.log files under $HOME or /tmp are considered; raw lines and secrets are never returned.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "log_dir": {
                        "type": "string",
                        "description": "Optional xtervo log directory under $HOME or /tmp; defaults to XTERVO_LOG_DIR/XDG state path"
                    }
                },
                "additionalProperties": false
            }
        }
    ]);
    // live 工具严格按 app 的 capabilities.commands 出现；server env 不参与
    // 列表决策。顺序与 mcp::ALL_COMMANDS 保持一致，便于 e2e 断言。
    if let Some(array) = tools.as_array_mut() {
        for command in mcp::ALL_COMMANDS {
            if !has(command) {
                continue;
            }
            if let Some(tool) = live_tool(command) {
                array.push(tool);
            }
        }
    }
    tools
}

/// 单个 live 工具的 MCP 定义。命令集合来自 app 的 capabilities。
fn live_tool(command: &str) -> Option<Value> {
    let mut tool = match command {
        "capabilities" => json!({
            "name": "xtervo.live_capabilities",
            "description": "Read the running xtervo MCP permission groups and commands.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        "snapshot" => json!({
            "name": "xtervo.live_snapshot",
            "description": "Read all pane frames/cursor/images metadata from the running xtervo live control socket (read-only).",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        "list" => json!({
            "name": "xtervo.live_list",
            "description": "List live panes, ids, grids, rectangles and focus (read-only).",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        "output" => json!({
            "name": "xtervo.live_output",
            "description": "Read bounded visible pane text. max_bytes is capped. Requires MCP read.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "max_bytes": {"type": "integer", "default": 65536}
            }, "required": ["pane_id"], "additionalProperties": false}
        }),
        "send_text" => json!({
            "name": "xtervo.live_send_text",
            "description": "Write text to a pane's PTY without pressing Enter. Requires MCP input.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "text": {"type": "string"}
            }, "required": ["pane_id", "text"], "additionalProperties": false}
        }),
        "send_key" => json!({
            "name": "xtervo.live_send_key",
            "description": "Send a named key/chord (Enter, Escape, Ctrl+C, Ctrl+D, Up/Down/Left/Right, Tab). Requires MCP input.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "key": {"type": "string"}
            }, "required": ["pane_id", "key"], "additionalProperties": false}
        }),
        "run" => json!({
            "name": "xtervo.live_run",
            "description": "Write one command line plus Enter to a pane. No shell is added by xtervo. Requires MCP input.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "command": {"type": "string"}
            }, "required": ["pane_id", "command"], "additionalProperties": false}
        }),
        "focus" => json!({
            "name": "xtervo.live_focus",
            "description": "Focus a pane. Requires MCP tabs.",
            "inputSchema": {"type": "object", "properties": {"pane_id": {"type": "integer"}}, "required": ["pane_id"]}
        }),
        "new_tab" => json!({
            "name": "xtervo.live_new_tab",
            "description": "Create a new xtervo tab. Requires MCP tabs.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        "split" => json!({
            "name": "xtervo.live_split",
            "description": "Split a pane right/down. Requires MCP tabs.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "direction": {"type": "string", "enum": ["right", "down"]}
            }, "required": ["pane_id"], "additionalProperties": false}
        }),
        "scroll" => json!({
            "name": "xtervo.live_scroll",
            "description": "Scroll a pane's scrollback by signed lines. Requires MCP tabs.",
            "inputSchema": {"type": "object", "properties": {
                "pane_id": {"type": "integer"},
                "lines": {"type": "integer"}
            }, "required": ["pane_id", "lines"], "additionalProperties": false}
        }),
        "close_pane" => json!({
            "name": "xtervo.live_close_pane",
            "description": "Close a pane. Requires MCP tabs + destructive.",
            "inputSchema": {"type": "object", "properties": {"pane_id": {"type": "integer"}}, "required": ["pane_id"]}
        }),
        "close_tab" => json!({
            "name": "xtervo.live_close_tab",
            "description": "Close the tab containing a pane. Requires MCP tabs + destructive.",
            "inputSchema": {"type": "object", "properties": {"pane_id": {"type": "integer"}}, "required": ["pane_id"]}
        }),
        "new_window" => json!({
            "name": "xtervo.live_new_window",
            "description": "Open a second in-process window (requires XTERVO_MULTI_WINDOW_INPROC=1). Requires MCP tabs.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        "close_window" => json!({
            "name": "xtervo.live_close_window",
            "description": "Close a window by window_id; refuses the last window. Requires MCP tabs + destructive.",
            "inputSchema": {"type": "object", "properties": {"window_id": {"type": "integer"}}}
        }),
        "kill" => json!({
            "name": "xtervo.live_kill",
            "description": "Send Ctrl+C to a pane. Requires MCP input + destructive.",
            "inputSchema": {"type": "object", "properties": {"pane_id": {"type": "integer"}}, "required": ["pane_id"]}
        }),
        _ => return None,
    };
    // 多窗口 additive 参数：所有 live 工具都接受可选 window_id（缺省 active）。
    if let Some(properties) = tool
        .get_mut("inputSchema")
        .and_then(|schema| schema.get_mut("properties"))
        .and_then(Value::as_object_mut)
    {
        properties.insert(
            "window_id".to_string(),
            json!({"type": "integer", "description": "Target WindowId; defaults to the active window."}),
        );
    }
    Some(tool)
}

fn mcp_call(req: &Value, live: &mut LiveTarget) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    if !params.is_object() {
        return mcp_error(-32602, "params must be an object");
    }
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return mcp_error(-32602, "arguments must be an object");
    }
    let result: Result<Value, Box<dyn Error>> = match name {
        "xtervo.diag_env" => Ok(env_json()),
        "xtervo.diag_snapshot" => {
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
                args.get("shell")
                    .and_then(Value::as_str)
                    .unwrap_or("/bin/sh")
                    .to_string(),
                "--".into(),
                "-c".into(),
                command,
            ]);
            snapshot_value(&cli)
        }
        "xtervo.diag_replay" => {
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
        }
        "xtervo.doctor_check" => mcp_ext::doctor_check(&args).map_err(|error| error.into()),
        "xtervo.issue_draft" => mcp_ext::issue_draft(&args).map_err(|error| error.into()),
        "xtervo.metrics_read" => mcp_ext::metrics_read(&args).map_err(|error| error.into()),
        "xtervo.live_capabilities" => live
            .exchange("capabilities", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_snapshot" => live
            .exchange("snapshot", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_list" => live.exchange("list", &args, false).map_err(|e| e.into()),
        "xtervo.live_output" => live.exchange("output", &args, false).map_err(|e| e.into()),
        "xtervo.live_send_text" => live
            .exchange("send_text", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_send_key" => live
            .exchange("send_key", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_run" => live.exchange("run", &args, false).map_err(|e| e.into()),
        "xtervo.live_focus" => live.exchange("focus", &args, false).map_err(|e| e.into()),
        "xtervo.live_new_tab" => live.exchange("new_tab", &args, false).map_err(|e| e.into()),
        "xtervo.live_new_window" => live
            .exchange("new_window", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_split" => live.exchange("split", &args, false).map_err(|e| e.into()),
        "xtervo.live_scroll" => live.exchange("scroll", &args, false).map_err(|e| e.into()),
        "xtervo.live_close_pane" => live
            .exchange("close_pane", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_close_tab" => live
            .exchange("close_tab", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_close_window" => live
            .exchange("close_window", &args, false)
            .map_err(|e| e.into()),
        "xtervo.live_kill" => live.exchange("kill", &args, false).map_err(|e| e.into()),
        _ => return mcp_error(-32602, "unknown tool"),
    };
    match result {
        Ok(value) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
            "isError": false,
        }),
        Err(e) => json!({
            "content": [{"type": "text", "text": format!("xtervo diag error: {e}")}],
            "isError": true,
        }),
    }
}

/// 与 app live socket 的一次请求/响应。`probe` 是 bridge 内部的权限探测，
/// 即使 read 分组关闭也允许（只返回分组/命令元数据，不返回 pane 内容）。
fn live_exchange(
    socket: Option<&str>,
    token: Option<&str>,
    cmd: &str,
    args: &Value,
    probe: bool,
) -> Result<Value, String> {
    let socket = socket.ok_or("live control socket 未配置（MCP 未启用，或 xtervo 未运行）")?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("连接 live socket {socket} 失败: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    use std::io::{BufRead, BufReader, Write};
    let mut req = serde_json::json!({"id": 1, "cmd": cmd, "token": token, "args": args});
    if probe {
        req["probe"] = json!(true);
    }
    writeln!(stream, "{req}").map_err(|e| format!("写 live socket 失败: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("读 live socket 失败: {e}"))?;
    let resp: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("live socket 响应不是 JSON: {e}"))?;
    if resp.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "live socket 拒绝: {}",
            resp.get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
}

fn mcp_error(code: i64, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

/// 已绑定的 live socket；`id` 用于关闭时只删除自己创建的 inode。
struct BoundSocket {
    listener: std::os::unix::net::UnixListener,
    path: std::path::PathBuf,
    id: (u64, u64),
}

/// Full live control: read commands plus host-side terminal actions.
///
/// 权限不再在启动时固化：`runtime` 里的分组可热更新，每个请求都重新读取。
/// 总开关关闭时 supervisor 会删除 socket；重新开启后自动重建。
pub fn start_control_socket_with_control<F, G>(
    build_snapshot: F,
    control: G,
    runtime: std::sync::Arc<mcp::McpRuntime>,
) where
    F: Fn() -> Value + Send + Sync + 'static,
    G: Fn(&str, &Value) -> Result<Value, String> + Send + Sync + 'static,
{
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let build = std::sync::Arc::new(build_snapshot);
    let control = std::sync::Arc::new(control);
    let spawned = std::thread::Builder::new()
        .name("mcp-socket".into())
        .spawn(move || {
            #[cfg(unix)]
            {
                let mut bound: Option<BoundSocket> = None;
                loop {
                    let permissions = runtime.permissions();
                    if !permissions.enabled {
                        if let Some(socket) = bound.take() {
                            drop(socket.listener);
                            remove_socket_if_same(&socket.path, socket.id);
                        }
                        runtime.set_active(false);
                        runtime.clear_error();
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    if bound.is_none() {
                        match bind_socket(&runtime) {
                            Ok(socket) => bound = Some(socket),
                            Err(error) => {
                                runtime.set_active(false);
                                runtime.set_error(Some(error));
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        }
                    }
                    let mut accept_error = None;
                    if let Some(socket) = &bound {
                        loop {
                            match socket.listener.accept() {
                                Ok((stream, _)) => {
                                    let build = build.clone();
                                    let control = control.clone();
                                    let runtime = runtime.clone();
                                    std::thread::Builder::new()
                                        .name("mcp-conn".into())
                                        .spawn(move || handle_conn(stream, build, control, runtime))
                                        .ok();
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    break;
                                }
                                Err(error) => {
                                    accept_error = Some(error);
                                    break;
                                }
                            }
                        }
                    }
                    if let Some(error) = accept_error {
                        runtime.set_error(Some(format!("live socket accept 失败: {error}")));
                        runtime.set_active(false);
                        if let Some(socket) = bound.take() {
                            drop(socket.listener);
                            remove_socket_if_same(&socket.path, socket.id);
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            #[cfg(not(unix))]
            {
                let _ = (&build, &control, &runtime);
                eprintln!("[diag] control socket 仅支持 Unix");
            }
        });
    if let Err(error) = spawned {
        eprintln!("[mcp] socket 线程启动失败: {error}");
        STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

fn bind_socket(runtime: &mcp::McpRuntime) -> Result<BoundSocket, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let path = runtime.socket_path().to_path_buf();
    mcp::prepare_socket_dir(&path)
        .map_err(|error| format!("创建 socket 目录失败 {}: {error}", path.display()))?;
    if path.exists() {
        // 另一个 xtervo 实例正在监听：不要抢/删它的 socket。
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => {
                return Err(format!(
                    "live socket 已被另一个 xtervo 实例占用: {}",
                    path.display()
                ))
            }
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|error| format!("live socket bind 失败 {}: {error}", path.display()))?;
    if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("live socket chmod 0600 失败: {error}"));
    }
    if let Err(error) = listener.set_nonblocking(true) {
        let _ = std::fs::remove_file(&path);
        return Err(format!("live socket nonblocking 失败: {error}"));
    }
    let metadata =
        std::fs::metadata(&path).map_err(|error| format!("live socket stat 失败: {error}"))?;
    runtime.set_active(true);
    runtime.clear_error();
    eprintln!(
        "[mcp] live socket ready path={} token={}",
        path.display(),
        if runtime.token().is_some() {
            "required"
        } else {
            "unavailable"
        }
    );
    Ok(BoundSocket {
        listener,
        path,
        id: (metadata.dev(), metadata.ino()),
    })
}

fn remove_socket_if_same(path: &std::path::Path, id: (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if (metadata.dev(), metadata.ino()) == id {
        let _ = std::fs::remove_file(path);
    }
}

fn handle_conn(
    mut stream: std::os::unix::net::UnixStream,
    build: std::sync::Arc<dyn Fn() -> Value + Send + Sync>,
    control: std::sync::Arc<dyn Fn(&str, &Value) -> Result<Value, String> + Send + Sync>,
    runtime: std::sync::Arc<mcp::McpRuntime>,
) {
    use std::io::{BufRead, BufReader, Write};

    fn reply(stream: &mut std::os::unix::net::UnixStream, id: &Value, data: Value) {
        let _ = writeln!(
            stream,
            "{}",
            serde_json::to_string(&json!({"id": id, "ok": true, "data": data})).unwrap()
        );
    }

    fn reply_error(stream: &mut std::os::unix::net::UnixStream, id: &Value, error: &str) {
        let _ = writeln!(
            stream,
            "{}",
            serde_json::to_string(&json!({"id": id, "ok": false, "error": error})).unwrap()
        );
    }

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
                let _ = writeln!(
                    stream,
                    "{}",
                    serde_json::to_string(&json!({"ok":false,"error":format!("bad json: {e}")}))
                        .unwrap()
                );
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let permissions = runtime.permissions();
        if !permissions.enabled {
            reply_error(
                &mut stream,
                &id,
                "MCP disabled (enable MCP in Settings → Privacy)",
            );
            continue;
        }
        // token 始终必需；state token 生成失败时也绝不放行。
        let expected = runtime.token();
        let authorized = match expected.as_deref() {
            Some(token) => req.get("token").and_then(Value::as_str) == Some(token),
            None => false,
        };
        if !authorized {
            reply_error(&mut stream, &id, "unauthorized");
            continue;
        }
        match req.get("cmd").and_then(Value::as_str) {
            Some("capabilities") => {
                let probe = req.get("probe").and_then(Value::as_bool).unwrap_or(false);
                if !probe && !permissions.allows_command("capabilities") {
                    reply_error(
                        &mut stream,
                        &id,
                        permissions
                            .denial_reason("capabilities")
                            .unwrap_or("read disabled"),
                    );
                    continue;
                }
                reply(&mut stream, &id, permissions.capabilities_json());
            }
            Some("snapshot") => {
                if !permissions.allows_command("snapshot") {
                    reply_error(
                        &mut stream,
                        &id,
                        permissions
                            .denial_reason("snapshot")
                            .unwrap_or("read disabled"),
                    );
                    continue;
                }
                let (tx, rx) = std::sync::mpsc::channel();
                let build = build.clone();
                let sent = slint::invoke_from_event_loop(move || {
                    let _ = tx.send(build());
                });
                if sent.is_err() {
                    reply_error(&mut stream, &id, "event loop closed");
                    continue;
                }
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(data) => reply(&mut stream, &id, data),
                    Err(_) => reply_error(&mut stream, &id, "snapshot timeout"),
                }
            }
            Some(command) => {
                if !permissions.allows_command(command) {
                    let audit_window = req
                        .get("args")
                        .and_then(|args| args.get("window_id"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    eprintln!("[mcp] denied action={command} window={audit_window}");
                    reply_error(
                        &mut stream,
                        &id,
                        permissions
                            .denial_reason(command)
                            .unwrap_or("command disabled"),
                    );
                    continue;
                }
                let args = req.get("args").cloned().unwrap_or_else(|| json!({}));
                let (tx, rx) = std::sync::mpsc::channel();
                let control = control.clone();
                let command = command.to_string();
                let sent = slint::invoke_from_event_loop(move || {
                    let _ = tx.send(control(&command, &args));
                });
                if sent.is_err() {
                    reply_error(&mut stream, &id, "event loop closed");
                    continue;
                }
                match rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(Ok(data)) => reply(&mut stream, &id, data),
                    Ok(Err(error)) => reply_error(&mut stream, &id, &error),
                    Err(_) => reply_error(&mut stream, &id, "live command timeout"),
                }
            }
            None => reply_error(&mut stream, &id, "missing cmd"),
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
                "xtervo-mcp-test-{}-{}-{stamp}",
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
        serve_reader_writer(
            Cursor::new(input.into_bytes()),
            &mut output,
            LiveTarget::default(),
        )
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
        let tools = mcp_tools(&[]);
        let names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"xtervo.diag_env"));
        assert!(names.contains(&"xtervo.diag_snapshot"));
        assert!(names.contains(&"xtervo.diag_replay"));
        assert!(names.contains(&"xtervo.doctor_check"));
        assert!(names.contains(&"xtervo.issue_draft"));
        assert!(names.contains(&"xtervo.metrics_read"));
        for tool in tools.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object");
        }

        let read_commands = vec![
            "capabilities".to_string(),
            "snapshot".to_string(),
            "list".to_string(),
            "output".to_string(),
        ];
        let live_tools = mcp_tools(&read_commands);
        let live_names: Vec<_> = live_tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(live_names.contains(&"xtervo.live_snapshot"));
        assert!(live_names.contains(&"xtervo.live_capabilities"));
        assert!(!live_names.contains(&"xtervo.live_send_text"));
        assert!(!live_names.contains(&"xtervo.live_close_pane"));
    }

    #[test]
    fn mcp_tools_follow_app_capability_commands_not_server_env() {
        // 只开放 input：列表只能出现 input 工具，不能出现 read/tabs/destructive。
        let input = vec![
            "send_text".to_string(),
            "send_key".to_string(),
            "run".to_string(),
        ];
        let input_tools = mcp_tools(&input);
        let names = tool_names(&input_tools);
        assert!(names.contains(&"xtervo.live_send_text"));
        assert!(names.contains(&"xtervo.live_send_key"));
        assert!(names.contains(&"xtervo.live_run"));
        assert!(!names.contains(&"xtervo.live_list"));
        assert!(!names.contains(&"xtervo.live_capabilities"));
        assert!(!names.contains(&"xtervo.live_close_pane"));

        // tabs + destructive：close 出现，kill（还要 input）不出现。
        let close = vec![
            "new_tab".to_string(),
            "new_window".to_string(),
            "split".to_string(),
            "close_pane".to_string(),
            "close_tab".to_string(),
            "close_window".to_string(),
        ];
        let close_tools = mcp_tools(&close);
        let names = tool_names(&close_tools);
        assert!(names.contains(&"xtervo.live_close_pane"));
        assert!(names.contains(&"xtervo.live_close_tab"));
        assert!(names.contains(&"xtervo.live_new_window"));
        assert!(names.contains(&"xtervo.live_close_window"));
        assert!(!names.contains(&"xtervo.live_kill"));
        assert!(!names.contains(&"xtervo.live_send_text"));

        // 空 commands = 只保留离线诊断工具。
        let empty_tools = mcp_tools(&[]);
        let names = tool_names(&empty_tools);
        assert!(!names.iter().any(|name| name.starts_with("xtervo.live_")));
    }

    fn tool_names(tools: &Value) -> Vec<&str> {
        tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn mcp_commands_from_capabilities_is_bounded_to_strings() {
        let value = json!({"commands": ["list", "output", 7, null]});
        assert_eq!(
            commands_from_capabilities(&value),
            Some(vec!["list".to_string(), "output".to_string()])
        );
        assert_eq!(commands_from_capabilities(&json!({})), None);
        assert_eq!(
            commands_from_capabilities(&json!({"commands": "list"})),
            None
        );
    }

    #[test]
    fn mcp_tools_list_uses_live_capabilities_probe() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = TestDir::new("cap-filter");
        let path = dir.join("live.sock");
        let listener = UnixListener::bind(&path).expect("bind fake live socket");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept probe");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read probe");
            let request: Value = serde_json::from_str(line.trim()).expect("probe json");
            assert_eq!(request["cmd"], "capabilities");
            assert_eq!(request["probe"], true);
            let mut stream = stream;
            writeln!(
                stream,
                "{}",
                json!({
                    "id": 1,
                    "ok": true,
                    "data": {"commands": ["list", "output"]}
                })
            )
            .unwrap();
        });
        let target = LiveTarget {
            socket: Some(path.display().to_string()),
            token: Some("test-token".into()),
            ..LiveTarget::default()
        };
        let mut output = Vec::new();
        serve_reader_writer(
            Cursor::new(
                mcp_request(1, "tools/list", json!({}))
                    .to_string()
                    .into_bytes(),
            ),
            &mut output,
            target,
        )
        .expect("serve fake live bridge");
        server.join().expect("fake server");
        let response: Value = serde_json::from_slice(&output).expect("tools/list json");
        let names = tool_names(&response["result"]["tools"]);
        assert!(names.contains(&"xtervo.live_list"));
        assert!(names.contains(&"xtervo.live_output"));
        assert!(!names.contains(&"xtervo.live_capabilities"));
        assert!(!names.contains(&"xtervo.live_send_text"));
    }

    #[test]
    fn mcp_initialize_advertises_tools_and_resources() {
        let responses = mcp_run(&[mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "xtervo-test", "version": "0"}
            }),
        )]);
        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["capabilities"]["resources"]["subscribe"], false);
        assert_eq!(result["capabilities"]["resources"]["listChanged"], false);
        assert_eq!(result["serverInfo"]["name"], "xtervo-diag");
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
                json!({"name": "xtervo.unknown_tool", "arguments": {}}),
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
            LiveTarget::default(),
        )
        .expect("serve malformed line");
        let response: Value = serde_json::from_slice(&output).expect("parse error response");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], -32700);
    }

    #[test]
    fn mcp_doctor_check_returns_diagnostic_case() {
        let response = mcp_call_tool("xtervo.doctor_check", json!({}));
        assert!(!mcp_is_error(&response));
        let case = mcp_tool_value(&response);
        assert_eq!(case["schema"], "xtervo.diagnostic_case.v1");
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
            "schema": "xtervo.diag.v1",
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
            "schema": "xtervo.diag.v1",
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
            "xtervo.doctor_check",
            json!({
                "frame": frame,
                "metrics": metrics,
                "symptom": "scroll residue",
                "repro_steps": ["open xtervo", "scroll"]
            }),
        );
        assert!(!mcp_is_error(&response));
        let case = mcp_tool_value(&response);
        assert_eq!(case["symptom"]["summary"], "scroll residue");
        assert_eq!(case["symptom"]["repro_steps"][0], "open xtervo");
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
                "schema": "xtervo.diag.v1",
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
                "schema": "xtervo.diag.v1",
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
            "xtervo.doctor_check",
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
                "schema": "xtervo.diag.v1",
                "kind": "env",
                "config": "/etc/passwd"
            }}),
        ] {
            let response = mcp_call_tool("xtervo.doctor_check", arguments);
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
        let check = mcp_call_tool("xtervo.doctor_check", json!({"symptom": "flicker"}));
        assert!(!mcp_is_error(&check));
        let case = mcp_tool_value(&check);
        let case_id = case["case_id"].clone();
        let response = mcp_call_tool("xtervo.issue_draft", json!({"case": case}));
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
        dir.write("xtervo-1.log", body);
        dir.write("notes.log", "API_KEY=super-secret\n");
        dir.write("xtervo-notlog.txt", "fatal secret\n");

        let response = mcp_call_tool("xtervo.metrics_read", json!({"log_dir": dir.as_str()}));
        assert!(!mcp_is_error(&response));
        let summary = mcp_tool_value(&response);
        assert_eq!(summary["exists"], true);
        assert_eq!(summary["summary"]["matched_files"], 1);
        assert_eq!(summary["summary"]["stats_files"], 1);
        assert_eq!(summary["summary"]["lines"], 3);
        assert_eq!(summary["summary"]["recent_errors"], 1);
        assert_eq!(summary["summary"]["bytes"], body.len() as u64);
        assert_eq!(summary["files"][0]["name"], "xtervo-1.log");

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
            let response = mcp_call_tool("xtervo.metrics_read", json!({"log_dir": path}));
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
            "xtervo.metrics_read",
            json!({"log_dir": link.to_string_lossy()}),
        );
        assert!(mcp_is_error(&response));
    }

    #[test]
    fn mcp_resources_expose_version_document() {
        let responses = mcp_run(&[
            mcp_request(1, "resources/list", json!({})),
            mcp_request(2, "resources/read", json!({"uri": "xtervo://version"})),
            mcp_request(3, "resources/read", json!({"uri": "xtervo://missing"})),
        ]);
        assert_eq!(
            responses[0]["result"]["resources"][0]["uri"],
            "xtervo://version"
        );
        let content = &responses[1]["result"]["contents"][0];
        assert_eq!(content["uri"], "xtervo://version");
        let version: Value =
            serde_json::from_str(content["text"].as_str().unwrap_or_default()).unwrap();
        assert_eq!(version["version"], VERSION);
        assert_eq!(version["schema"], "xtervo.diag.v1");
        assert_eq!(responses[2]["error"]["code"], -32002);
    }

    #[test]
    fn mcp_read_only_tools_do_not_write_files() {
        let dir = TestDir::new("readonly");
        let config = dir.write("config.toml", "theme = \"dark\"\n");
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).expect("create logs dir");
        std::fs::write(logs.join("xtervo-1.log"), "one\ntwo\n").expect("write log");
        let before = dir.snapshot();

        let environment = json!({
            "schema": "xtervo.diag.v1",
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
        let check = mcp_call_tool("xtervo.doctor_check", json!({"environment": environment}));
        assert!(!mcp_is_error(&check));
        let case = mcp_tool_value(&check);
        let draft = mcp_call_tool("xtervo.issue_draft", json!({"case": case}));
        assert!(!mcp_is_error(&draft));
        let metrics = mcp_call_tool(
            "xtervo.metrics_read",
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
        assert_eq!(value["schema"], "xtervo.diag.v1");
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
            "schema": "xtervo.diag.v1",
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
            "schema": "xtervo.diag.v1",
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
        assert_eq!(value["schema"], "xtervo.diag.v1");
        assert_eq!(value["kind"], "scene_diff");
        assert_eq!(value["images"]["counts"]["moved"], 1);
    }

    #[test]
    fn observability_document_is_additive_and_marks_unfed_collector() {
        let value = observability_value("observability");
        assert_eq!(value["schema"], "xtervo.diag.v1");
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
