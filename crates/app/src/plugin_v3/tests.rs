// SPDX-License-Identifier: GPL-3.0-only
//! Plugin API v3 宿主测试。
//!
//! 所有集成测试都使用真实 Python 子进程 + JSON-RPC 2.0 stdio，不 mock supervisor。
//! 测试超时被缩短（[`SupervisorOptions`]），生产默认值不变。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use xtervo_core::{PaneConfig, PaneHandle};

use super::manifest::{HOST_ABI, PLUGIN_API_VERSION};
use super::*;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "xtervo-plugin-v3-{}-{tag}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fast_options() -> SupervisorOptions {
    SupervisorOptions {
        handshake_timeout: Duration::from_secs(2),
        command_timeout: Duration::from_secs(2),
        status_interval: Duration::from_millis(100),
        status_timeout: Duration::from_millis(400),
        ping_interval: Duration::from_secs(60),
        ping_timeout: Duration::from_secs(1),
        surface_timeout: Duration::from_secs(1),
        approval_timeout: Duration::from_secs(5),
        max_restarts: 3,
        backoff_base: Duration::from_millis(20),
        backoff_max: Duration::from_millis(100),
        max_bad_lines: 1,
        max_line_bytes: 256 * 1024,
        max_output_bytes: protocol::MAX_OUTPUT_BYTES,
        max_stderr_bytes: 8 * 1024,
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

const COMMANDS: &[&str] = &[
    "greet",
    "slow",
    "badjson",
    "longline",
    "terminal-read",
    "terminal-read-window",
    "terminal-read-fake-windows",
    "terminal-write",
    "notify-window",
    "status-window",
    "spam-fake-status",
    "stderr-crash",
    "panel-window",
    "clipboard-read",
    "push-status",
    "context",
    "notify",
    "panel",
    "credentials",
];

fn manifest(
    id: &str,
    version: &str,
    terminal_context: bool,
    terminal_write: bool,
    status: bool,
) -> String {
    let commands: String = COMMANDS
        .iter()
        .map(|command| {
            // "context" 声明为菜单装饰项（UI-TERM-013）：宿主菜单应能看到它。
            let menu = if *command == "context" {
                "menu = true\n"
            } else {
                ""
            };
            format!("[[commands]]\nid = \"{command}\"\ntitle = \"{command}\"\n{menu}")
        })
        .collect();
    format!(
        r#"api = 3
host_abi = 3
id = "{id}"
name = "Test {id}"
version = "{version}"
runtime = "process"
entry = "main.py"
description = "integration test plugin"

[permissions]
filesystem = "plugin"
network = false
process = false
clipboard = false
terminal_context = {terminal_context}
terminal_write = {terminal_write}
credentials = "own"

[contributes]
commands = true
status = {status}
events = ["startup", "tab-open", "tab-close", "pane-focus"]
panels = ["main"]
settings = true
assistant = false

{commands}"#
    )
}

const PLUGIN_SCRIPT: &str = r####"#!/usr/bin/env python3
import json
import os
import sys
import time

STATUS_DELAY = __STATUS_DELAY__
HANDSHAKE_DELAY = __HANDSHAKE_DELAY__
IGNORE_PING = False
FREEZE_AFTER_INIT = False
PROTOCOL = 3
_next = 100
_pending = {}

def send(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def respond(rid, result):
    send({"jsonrpc": "2.0", "id": rid, "result": result})

def request(method, params):
    global _next
    rid = _next
    _next += 1
    send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
    return rid

def handle_response(msg):
    pending = _pending.pop(msg.get("id"), None)
    if pending is None:
        return
    kind, command_id, window = pending
    if "error" in msg:
        error = msg["error"]
        respond(command_id, {"ok": False, "message": "%s: %s" % (kind, error.get("message", "error"))})
    else:
        result = msg.get("result") or {}
        if kind == "terminal.read":
            text = str(result.get("text", ""))
            if window is not None:
                send({"jsonrpc": "2.0", "method": "ui.notify", "params": {
                    "text": "READWIN[%s]:%s" % (window, text),
                    "level": "info",
                    "window_id": window,
                }})
                respond(command_id, {"ok": True, "message": "READWIN[%s]" % window})
            else:
                respond(command_id, {"ok": True, "message": "READ:" + text})
        elif kind == "context.version":
            respond(command_id, {"ok": True, "message": "HOST:" + str(result.get("host_version"))})
        elif kind == "clipboard.read":
            respond(command_id, {"ok": True, "message": "CLIP:" + str(result.get("text", ""))})
        elif kind == "ui.render":
            respond(command_id, {"ok": True, "message": "UI_ACCEPTED"})
        else:
            respond(command_id, {"ok": True, "message": "OK"})

def handle(msg):
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        if HANDSHAKE_DELAY:
            # 模拟慢握手：宿主启动早期派发的 startup 事件会先到 supervisor 队列。
            time.sleep(HANDSHAKE_DELAY)
        respond(rid, {"protocol_version": PROTOCOL})
        if FREEZE_AFTER_INIT:
            # 模拟停读 stdin 的插件：握手后不再消费宿主写入。
            time.sleep(3600)
    elif method == "initialized":
        pass
    elif method == "command.invoke":
        command = params.get("command")
        window = params.get("window_id")
        if command == "greet":
            respond(rid, {"ok": True, "message": "hello-v3"})
        elif command == "slow":
            time.sleep(5)
            respond(rid, {"ok": True, "message": "late"})
        elif command == "badjson":
            sys.stdout.write("this-is-not-json\nthis-is-not-json\n")
            sys.stdout.flush()
        elif command == "longline":
            sys.stdout.write("x" * 20000 + "\n")
            sys.stdout.flush()
        elif command == "terminal-read":
            pending = request("terminal.read", {"max_lines": 5, "max_bytes": 4096})
            _pending[pending] = ("terminal.read", rid, None)
        elif command == "terminal-read-window":
            body = {"max_lines": 5, "max_bytes": 4096}
            if window is not None:
                body["window_id"] = window
            pending = request("terminal.read", body)
            _pending[pending] = ("terminal.read", rid, window)
        elif command == "terminal-read-fake-windows":
            # 伪造窗口 id 的 ask：不应占用审批队列，也不应阻止随后合法窗口的 ask。
            for index in range(20):
                request("terminal.read", {
                    "max_lines": 1,
                    "max_bytes": 16,
                    "window_id": 1000 + index,
                })
            respond(rid, {"ok": True, "message": "fake-windows-sent"})
        elif command == "spam-fake-status":
            for index in range(10000):
                send({"jsonrpc": "2.0", "method": "ui.status", "params": {
                    "text": "spam-status",
                    "command": "greet",
                    "window_id": 10000 + index,
                }})
            respond(rid, {"ok": True, "message": "spam-status-sent"})
        elif command == "stderr-crash":
            sys.stderr.write("\x1b[31mred-alert\x1b[0m\rEVIL\r\n")
            sys.stderr.write("second\x1b[2Jline\n")
            sys.stderr.flush()
            # 给 stderr 读线程时间读取，再退出触发 crash 路径。
            time.sleep(0.25)
            sys.exit(7)
        elif command == "terminal-write":
            body = {"text": "echo hello\n"}
            if window is not None:
                body["window_id"] = window
            pending = request("terminal.write", body)
            _pending[pending] = ("terminal.write", rid, window)
        elif command == "clipboard-read":
            pending = request("clipboard.read", {})
            _pending[pending] = ("clipboard.read", rid, None)
        elif command == "context":
            pending = request("context.version", {})
            _pending[pending] = ("context.version", rid, None)
        elif command == "notify":
            send({"jsonrpc": "2.0", "method": "ui.notify", "params": {"text": "plugin-notify", "level": "info"}})
            respond(rid, {"ok": True, "message": "notified"})
        elif command == "notify-window":
            body = {"text": "notify-window", "level": "info"}
            if window is not None:
                body["window_id"] = window
            send({"jsonrpc": "2.0", "method": "ui.notify", "params": body})
            respond(rid, {"ok": True, "message": "notified-window"})
        elif command == "status-window":
            body = {"text": "status-window-%s" % window, "command": "greet"}
            if window is not None:
                body["window_id"] = window
            send({"jsonrpc": "2.0", "method": "ui.status", "params": body})
            respond(rid, {"ok": True, "message": "status-window"})
        elif command == "panel-window":
            elements = [{"type": "text", "text": "panel-window-%s" % window}]
            body = {"surface": "panel", "panel": "main", "title": "PANEL[%s]" % window, "elements": elements}
            if window is not None:
                body["window_id"] = window
            pending = request("ui.render", body)
            _pending[pending] = ("ui.render", rid, window)
        elif command == "push-status":
            send({"jsonrpc": "2.0", "method": "ui.status", "params": {"text": "pushed-status", "command": "greet"}})
            respond(rid, {"ok": True, "message": "status pushed"})
        elif command == "panel":
            elements = [
                {"type": "text", "text": "panel-text"},
                {"type": "markdown", "text": "## md"},
                {"type": "list", "items": ["a", "b"]},
                {"type": "input", "id": "name", "label": "Name", "value": "v"},
                {"type": "button", "id": "run", "label": "Run"},
                {"type": "select", "id": "mode", "label": "Mode", "value": "a", "options": ["a", "b"]},
                {"type": "progress", "label": "P", "value": 0.5},
            ]
            pending = request("ui.render", {"surface": "panel", "panel": "main", "title": "Test panel", "elements": elements})
            _pending[pending] = ("ui.render", rid, None)
        elif command == "credentials":
            pending = request("credentials.get", {})
            _pending[pending] = ("credentials.get", rid, None)
        else:
            respond(rid, {"ok": False, "message": "unknown command"})
    elif method == "status.get":
        if STATUS_DELAY:
            time.sleep(STATUS_DELAY)
        respond(rid, {"text": "status-ready", "tooltip": "tip", "command": "greet"})
    elif method == "ping":
        if not IGNORE_PING:
            respond(rid, {"seq": params.get("seq", 0)})
    elif method == "surface.render":
        elements = [
            {"type": "text", "text": "surface-text"},
            {"type": "markdown", "text": "# surface-md"},
            {"type": "list", "items": ["x", "y"]},
            {"type": "input", "id": "name", "label": "Name", "value": "v"},
            {"type": "button", "id": "run", "label": "Run"},
            {"type": "select", "id": "mode", "label": "Mode", "value": "a", "options": ["a", "b"]},
            {"type": "progress", "label": "P", "value": 0.4},
        ]
        respond(rid, {"version": 1, "title": "Test surface", "elements": elements})
    elif method == "shutdown":
        respond(rid, {"ok": True})
        sys.exit(0)
    elif method == "ui.action":
        pass
    elif method == "event.deliver":
        with open("events.txt", "a", encoding="utf-8") as file:
            file.write(str(params.get("event")) + "\n")
    else:
        if rid is not None:
            send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown"}})

def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        if "method" in message:
            handle(message)
        else:
            handle_response(message)

if __name__ == "__main__":
    os.chdir(os.path.dirname(os.path.abspath(__file__)))
    main()
"####;

fn install_plugin(root: &Path, dir_name: &str, manifest_body: &str, status_delay: f32) -> PathBuf {
    install_plugin_with_handshake_delay(root, dir_name, manifest_body, status_delay, 0.0)
}

fn install_plugin_with_handshake_delay(
    root: &Path,
    dir_name: &str,
    manifest_body: &str,
    status_delay: f32,
    handshake_delay: f32,
) -> PathBuf {
    let dir = root.join(dir_name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest_body).unwrap();
    let script = PLUGIN_SCRIPT
        .replace("__STATUS_DELAY__", &status_delay.to_string())
        .replace("__HANDSHAKE_DELAY__", &handshake_delay.to_string());
    write_executable(&dir.join("main.py"), &script);
    dir
}

fn load(root: &Path, state: &Path, options: SupervisorOptions) -> PluginStore {
    let mut store = PluginStore::load_with_options(root, state, options);
    // 插件测试会 spawn 真实 sidecar；并行跑几十个会 EMFILE/抢 CPU，导致
    // 偶发“进程异常退出/重启”假失败。这里串行化所有 load() 测试。
    if let Ok(guard) = super::TEST_SIDECAR_LOCK.try_lock() {
        store.test_guard = Some(guard);
    }
    store
}

fn enable(store: &mut PluginStore, key: &str) {
    match store.request_enable(key).unwrap() {
        EnableDecision::Approved => {}
        EnableDecision::NeedsApproval { .. } => store.confirm_enable(key).unwrap(),
    }
}

fn wait_until<F: FnMut() -> bool>(mut predicate: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    predicate()
}

fn wait_phase(store: &PluginStore, key: &str, phase: Phase, timeout: Duration) -> bool {
    wait_until(|| store.phase(key) == Some(phase), timeout)
}

fn activity_contains(store: &PluginStore, needle: &str) -> bool {
    store
        .activity_snapshot()
        .1
        .iter()
        .any(|line| line.contains(needle))
}

fn wait_activity(store: &PluginStore, needle: &str, timeout: Duration) -> bool {
    wait_until(|| activity_contains(store, needle), timeout)
}

fn row<'a>(rows: &'a [PluginRow], key: &str) -> &'a PluginRow {
    rows.iter().find(|row| row.key == key).expect("row exists")
}

fn row_of(store: &PluginStore, key: &str) -> PluginRow {
    store
        .rows()
        .into_iter()
        .find(|row| row.key == key)
        .expect("row exists")
}

#[test]
fn example_manifest_is_valid_v3_and_executable() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/plugin-v3-echo/plugin.toml");
    let text = std::fs::read_to_string(&path).expect("example manifest exists");
    let manifest: Manifest = toml::from_str(&text).expect("example manifest parses");
    manifest.validate().expect("example manifest validates");
    assert_eq!(manifest.api, PLUGIN_API_VERSION);
    assert_eq!(manifest.host_abi, HOST_ABI);
    assert!(manifest.contributes.settings);
    assert_eq!(manifest.contributes.panels, vec!["echo".to_string()]);
    let dir = path.parent().unwrap();
    let entry = super::manifest::resolve_entry(dir, &manifest.entry).unwrap();
    assert!(entry.is_file());
}

#[test]
fn docs_example_plugin_runs_full_flow() {
    let root = tmpdir("example");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/plugin-v3-echo");
    let dest = root.join("echo");
    std::fs::create_dir_all(&dest).unwrap();
    for name in ["plugin.toml", "main.py"] {
        std::fs::copy(source.join(name), dest.join(name)).unwrap();
    }
    // fs::copy 在部分平台不保证执行位；显式设置。
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(dest.join("main.py"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(dest.join("main.py"), permissions).unwrap();

    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.xtervo.echo");
    assert!(wait_phase(
        &store,
        "dev.xtervo.echo",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.xtervo.echo.greet"));
    assert!(wait_activity(
        &store,
        "Hello from Echo Plugin API v3",
        Duration::from_secs(3)
    ));
    assert!(wait_until(
        || store
            .status_snapshot()
            .1
            .iter()
            .any(|status| status.text == "Echo v3 ready"),
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.xtervo.echo.notify"));
    assert!(wait_until(
        || store
            .take_notices()
            .iter()
            .any(|notice| notice.text.contains("Echo says hi")),
        Duration::from_secs(3)
    ));
    store
        .request_surface("dev.xtervo.echo", "settings", None)
        .unwrap();
    assert!(wait_until(
        || {
            store
                .take_surfaces()
                .iter()
                .any(|surface| surface.surface == "settings")
        },
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.xtervo.echo.read-terminal"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert!(store.resolve_host_request(
        "dev.xtervo.echo",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "example-tail"})),
    ));
    assert!(wait_activity(
        &store,
        "example-tail",
        Duration::from_secs(3)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discovery_classifies_v3_and_v1_v2_without_executing() {
    let root = tmpdir("discovery");
    install_plugin(
        &root,
        "good",
        &manifest("dev.test.good", "1.0.0", false, false, false),
        0.0,
    );
    let legacy = root.join("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("plugin.toml"),
        "name = \"legacy\"\nversion = \"0.9\"\napi = 2\nruntime = \"process\"\nentry = \"main.py\"\n",
    )
    .unwrap();
    let marker = legacy.join("ran");
    write_executable(
        &legacy.join("main.py"),
        &format!("open({:?}, 'w').write('ran')\n", marker.to_string_lossy()),
    );
    let flat = root.join("old-flat.toml");
    std::fs::write(&flat, "id = \"old\"\nname = \"Old\"\nversion = \"0.1\"\n").unwrap();

    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    store.start_process_plugins();
    std::thread::sleep(Duration::from_millis(150));
    assert!(!marker.exists(), "v1/v2 plugins must never be spawned");
    let rows = store.rows();
    assert_eq!(rows.len(), 3);
    let good = row(&rows, "dev.test.good");
    assert!(good.supported);
    assert_eq!(good.api, "3");
    let legacy_row = rows.iter().find(|row| row.name == "legacy").unwrap();
    assert!(!legacy_row.supported);
    assert_eq!(legacy_row.status, "unsupported");
    assert!(
        legacy_row.error.contains("v1/v2") && legacy_row.error.contains("api = 3"),
        "{}",
        legacy_row.error
    );
    assert_eq!(legacy_row.api, "2");
    let flat_row = rows.iter().find(|row| row.name == "Old").unwrap();
    assert!(!flat_row.supported);
    assert_eq!(flat_row.status, "unsupported");
    assert!(store.request_enable("legacy:legacy").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn first_enable_requires_confirmation_and_persists_approval() {
    let root = tmpdir("approval");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.approval", "1.0.0", true, false, false),
        0.0,
    );
    let state_path = root.join("state.toml");
    let mut store = load(&root, &state_path, fast_options());
    match store.request_enable("dev.test.approval").unwrap() {
        EnableDecision::NeedsApproval { permissions } => {
            assert!(permissions.contains(&"terminal_context".to_string()));
        }
        EnableDecision::Approved => panic!("first enable must ask for confirmation"),
    }
    assert!(
        !state_path.exists(),
        "a pending confirmation must not enable"
    );
    assert!(store.phase("dev.test.approval").is_none());
    let entry_row = row_of(&store, "dev.test.approval");
    assert!(!entry_row.enabled);
    assert_eq!(entry_row.status, "disabled");

    store.confirm_enable("dev.test.approval").unwrap();
    assert!(wait_phase(
        &store,
        "dev.test.approval",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.is_approved("dev.test.approval"));
    assert!(row_of(&store, "dev.test.approval").enabled);
    let persisted = std::fs::read_to_string(&state_path).unwrap();
    assert!(persisted.contains("dev.test.approval"));
    assert!(!persisted.contains("secret"));
    assert!(!persisted.contains(std::env::var("HOME").unwrap_or_default().as_str()));

    store.shutdown_processes();
    drop(store);
    // 重新加载状态：已确认 + enabled 的插件应自动启动，无需再次确认。
    let mut store = load(&root, &state_path, fast_options());
    store.start_process_plugins();
    assert!(wait_phase(
        &store,
        "dev.test.approval",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.disable_plugin("dev.test.approval").unwrap();
    assert_eq!(row_of(&store, "dev.test.approval").status, "disabled");
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn startup_event_dispatched_before_handshake_is_queued_until_initialized() {
    let root = tmpdir("startup-before-handshake");
    install_plugin_with_handshake_delay(
        &root,
        "plugin",
        &manifest("dev.test.startup", "1.0.0", false, false, false),
        0.0,
        0.4,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.startup");
    // main.rs 在 supervisor 还处于 Starting 时就派发 startup；旧实现因
    // initialized=false 直接丢弃，插件永远收不到 startup（docs §7 要求排队）。
    assert!(store.spawn_event(PluginEvent::Startup, &json!({})) > 0);
    let events = root.join("plugin/events.txt");
    assert!(
        wait_until(
            || std::fs::read_to_string(&events)
                .map(|text| text.lines().any(|line| line == "startup"))
                .unwrap_or(false),
            Duration::from_secs(3)
        ),
        "startup 事件必须在 initialized 之后补发，不能因握手竞态丢失"
    );
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lifecycle_command_status_ping_context_and_events() {
    let root = tmpdir("lifecycle");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.lifecycle", "1.0.0", false, false, true),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.lifecycle");
    assert!(wait_phase(
        &store,
        "dev.test.lifecycle",
        Phase::Running,
        Duration::from_secs(3)
    ));

    assert!(store.spawn_command("plugin.dev.test.lifecycle.greet"));
    assert!(wait_activity(&store, "hello-v3", Duration::from_secs(3)));
    assert!(store
        .take_notices()
        .iter()
        .any(|notice| notice.text.contains("hello-v3")));

    assert!(wait_until(
        || !store.status_snapshot().1.is_empty(),
        Duration::from_secs(3)
    ));
    let status_items = store.status_snapshot().1;
    let status = status_items.first().unwrap();
    assert_eq!(status.text, "status-ready");
    assert_eq!(
        status.command.as_deref(),
        Some("plugin.dev.test.lifecycle.greet")
    );

    assert!(store.spawn_command("plugin.dev.test.lifecycle.push-status"));
    assert!(wait_until(
        || store
            .status_snapshot()
            .1
            .iter()
            .any(|status| status.text == "pushed-status"),
        Duration::from_secs(3)
    ));

    assert!(store.spawn_command("plugin.dev.test.lifecycle.context"));
    assert!(wait_activity(&store, "HOST:", Duration::from_secs(3)));

    assert!(store.spawn_event(PluginEvent::Startup, &json!({})) > 0);
    assert!(store.spawn_event(PluginEvent::TabOpen, &tab_open_payload("demo", "local")) > 0);
    assert!(store.spawn_event(PluginEvent::PaneFocus, &pane_focus_payload(7)) > 0);
    let events = root.join("plugin/events.txt");
    assert!(wait_until(
        || std::fs::read_to_string(&events)
            .map(|text| {
                text.lines().any(|line| line == "startup")
                    && text.lines().any(|line| line == "tab-open")
                    && text.lines().any(|line| line == "pane-focus")
            })
            .unwrap_or(false),
        Duration::from_secs(3)
    ));

    store.shutdown_processes();
    assert!(wait_until(
        || store
            .phase("dev.test.lifecycle")
            .is_none_or(|phase| phase == Phase::Stopped),
        Duration::from_secs(2)
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bad_json_triggers_restart_not_terminal_failure() {
    let root = tmpdir("badjson");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.badjson", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.max_bad_lines = 1;
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.badjson");
    assert!(wait_phase(
        &store,
        "dev.test.badjson",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.badjson.badjson"));
    assert!(wait_until(
        || store
            .runtime_info("dev.test.badjson")
            .is_some_and(|info| info.restart_count >= 1),
        Duration::from_secs(4)
    ));
    let _ = store.rows();
    assert!(wait_until(
        || store.phase("dev.test.badjson") == Some(Phase::Running),
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.badjson.greet"));
    assert!(wait_activity(&store, "hello-v3", Duration::from_secs(3)));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn command_timeout_reports_timeout_and_restarts() {
    let root = tmpdir("cmdtimeout");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.timeout", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.command_timeout = Duration::from_millis(200);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.timeout");
    assert!(wait_phase(
        &store,
        "dev.test.timeout",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.timeout.slow"));
    assert!(wait_activity(&store, "timeout", Duration::from_secs(3)));
    assert!(wait_until(
        || store
            .runtime_info("dev.test.timeout")
            .is_some_and(|info| info.restart_count >= 1),
        Duration::from_secs(4)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ping_success_is_quiet_and_consecutive_timeouts_restart() {
    let root = tmpdir("pingok");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.pingok", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.ping_interval = Duration::from_millis(50);
    options.ping_timeout = Duration::from_millis(100);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.pingok");
    assert!(wait_phase(
        &store,
        "dev.test.pingok",
        Phase::Running,
        Duration::from_secs(3)
    ));
    std::thread::sleep(Duration::from_millis(350));
    let info = store.runtime_info("dev.test.pingok").unwrap();
    assert_eq!(info.phase(), Phase::Running);
    assert_eq!(info.restart_count, 0);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);

    let root = tmpdir("pingfail");
    let dir = install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.pingfail", "1.0.0", false, false, false),
        0.0,
    );
    // 让测试插件忽略 ping；连续 2 次超时后 supervisor 必须重启。
    let script = std::fs::read_to_string(dir.join("main.py"))
        .unwrap()
        .replace("IGNORE_PING = False", "IGNORE_PING = True");
    write_executable(&dir.join("main.py"), &script);
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.ping_interval = Duration::from_millis(50);
    options.ping_timeout = Duration::from_millis(80);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.pingfail");
    assert!(wait_phase(
        &store,
        "dev.test.pingfail",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(wait_until(
        || store
            .runtime_info("dev.test.pingfail")
            .is_some_and(|info| info.restart_count >= 1),
        Duration::from_secs(4)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn status_timeout_does_not_kill_process() {
    let root = tmpdir("statustimeout");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.statustimeout", "1.0.0", false, false, true),
        0.5,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.status_interval = Duration::from_millis(50);
    options.status_timeout = Duration::from_millis(100);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.statustimeout");
    assert!(wait_phase(
        &store,
        "dev.test.statustimeout",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let pid = store
        .runtime_info("dev.test.statustimeout")
        .and_then(|info| info.pid)
        .expect("pid");
    std::thread::sleep(Duration::from_millis(700));
    let info = store.runtime_info("dev.test.statustimeout").unwrap();
    assert_eq!(info.phase(), Phase::Running);
    assert_eq!(info.pid, Some(pid));
    assert_eq!(info.restart_count, 0);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oversized_line_and_output_cap_stop_runaway_plugin() {
    let root = tmpdir("linelimit");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.line", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.max_line_bytes = 1024;
    options.max_bad_lines = 0;
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.line");
    assert!(wait_phase(
        &store,
        "dev.test.line",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.line.longline"));
    assert!(wait_until(
        || store
            .runtime_info("dev.test.line")
            .is_some_and(|info| info.restart_count >= 1),
        Duration::from_secs(4)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);

    // 总输出上限 + max_restarts=0：最终 Failed，而不是无限重启。
    let root = tmpdir("outputcap");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.output", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.max_output_bytes = 1024;
    options.max_restarts = 0;
    options.max_bad_lines = 5;
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.output");
    assert!(wait_phase(
        &store,
        "dev.test.output",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.output.longline"));
    assert!(wait_phase(
        &store,
        "dev.test.output",
        Phase::Failed,
        Duration::from_secs(4)
    ));
    let row = row_of(&store, "dev.test.output");
    assert_eq!(row.status, "failed");
    assert!(
        row.error.contains("输出上限") || row.error.contains("重启上限"),
        "{}",
        row.error
    );
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn terminal_read_requires_capability_and_per_request_allow() {
    let root = tmpdir("terminalread");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.terminal", "1.0.0", true, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.terminal");
    assert!(wait_phase(
        &store,
        "dev.test.terminal",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.terminal.terminal-read"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert_eq!(approval.method, "terminal.read");
    assert_eq!(approval.plugin_id, "dev.test.terminal");
    assert!(approval.summary.contains("terminal"));
    assert!(store.resolve_host_request(
        "dev.test.terminal",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "tail-data"})),
    ));
    assert!(wait_activity(
        &store,
        "READ:tail-data",
        Duration::from_secs(3)
    ));
    assert!(store.pending_approvals().is_empty());

    // 拒绝路径：插件收到 denied by user，命令失败但进程继续运行。
    assert!(store.spawn_command("plugin.dev.test.terminal.terminal-read"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert!(store.resolve_host_request(
        "dev.test.terminal",
        approval.request_id,
        HostOutcome::Denied,
    ));
    assert!(wait_activity(&store, "denied", Duration::from_secs(3)));
    assert_eq!(store.phase("dev.test.terminal"), Some(Phase::Running));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);

    // 没有 terminal_context：连 ask 都不会出现，直接 capability deny。
    let root = tmpdir("terminaldenied");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.noperm", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.noperm");
    assert!(wait_phase(
        &store,
        "dev.test.noperm",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.noperm.terminal-read"));
    assert!(wait_activity(&store, "denied", Duration::from_secs(3)));
    assert!(store.pending_approvals().is_empty());
    assert_eq!(store.phase("dev.test.noperm"), Some(Phase::Running));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn terminal_write_is_ask_gated_and_credentials_are_never_hosted() {
    let root = tmpdir("terminalwrite");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.write", "1.0.0", false, true, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.write");
    assert!(wait_phase(
        &store,
        "dev.test.write",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.write.terminal-write"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert_eq!(approval.method, "terminal.write");
    assert!(approval.details.contains("echo hello"));
    assert!(store.resolve_host_request(
        "dev.test.write",
        approval.request_id,
        HostOutcome::Ok(json!({})),
    ));
    assert!(wait_activity(
        &store,
        "terminal-write",
        Duration::from_secs(3)
    ));

    assert!(store.spawn_command("plugin.dev.test.write.credentials"));
    assert!(wait_activity(
        &store,
        "credentials.get",
        Duration::from_secs(3)
    ));
    assert!(activity_contains(&store, "凭据") || activity_contains(&store, "Vault"));
    assert_eq!(store.phase("dev.test.write"), Some(Phase::Running));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn declarative_ui_and_notifications_flow_through_host() {
    let root = tmpdir("ui");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.ui", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.ui");
    assert!(wait_phase(
        &store,
        "dev.test.ui",
        Phase::Running,
        Duration::from_secs(3)
    ));

    assert!(store.spawn_command("plugin.dev.test.ui.notify"));
    assert!(wait_until(
        || store
            .take_notices()
            .iter()
            .any(|notice| notice.text.contains("plugin-notify")),
        Duration::from_secs(3)
    ));

    store
        .request_surface("dev.test.ui", "settings", None)
        .unwrap();
    let mut collected: Vec<SurfaceUpdate> = Vec::new();
    assert!(wait_until(
        || {
            let updates = store.take_surfaces();
            let found = updates.iter().any(|surface| surface.surface == "settings");
            collected.extend(updates);
            found
        },
        Duration::from_secs(3)
    ));
    let surface = collected
        .into_iter()
        .find(|surface| surface.surface == "settings")
        .expect("settings surface");
    assert_eq!(surface.doc.elements.len(), 7);
    assert!(surface
        .doc
        .elements
        .iter()
        .any(|element| matches!(element, SurfaceElement::Input { .. })));
    assert!(surface
        .doc
        .elements
        .iter()
        .any(|element| matches!(element, SurfaceElement::Select { .. })));
    assert!(surface
        .doc
        .elements
        .iter()
        .any(|element| matches!(element, SurfaceElement::Progress { .. })));
    assert!(surface
        .doc
        .elements
        .iter()
        .any(|element| matches!(element, SurfaceElement::Button { .. })));

    assert!(store.spawn_command("plugin.dev.test.ui.panel"));
    let mut surfaces: Vec<SurfaceUpdate> = Vec::new();
    assert!(wait_until(
        || {
            let updates = store.take_surfaces();
            let found = updates
                .iter()
                .any(|surface| surface.surface == "panel:main");
            surfaces.extend(updates);
            found
        },
        Duration::from_secs(3)
    ));
    assert!(surfaces
        .iter()
        .any(|surface| surface.surface == "panel:main"));
    assert!(store.send_ui_action(
        "dev.test.ui",
        "panel:main",
        Some("main".to_string()),
        "run",
        "click",
        None,
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reload_refreshes_manifest_without_restarting_host_and_permission_change_requires_confirm() {
    let root = tmpdir("reload");
    let dir = install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.reload", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.reload");
    assert!(wait_phase(
        &store,
        "dev.test.reload",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let first_pid = store
        .runtime_info("dev.test.reload")
        .and_then(|info| info.pid);
    assert!(first_pid.is_some());

    let mut new_manifest: Manifest =
        toml::from_str(&manifest("dev.test.reload", "1.1.0", false, false, false)).unwrap();
    new_manifest.commands[0].title = "Reloaded Greet".to_string();
    std::fs::write(
        dir.join("plugin.toml"),
        toml::to_string_pretty(&new_manifest).unwrap(),
    )
    .unwrap();
    store.reload_plugin("dev.test.reload").unwrap();
    assert!(wait_until(
        || row_of(&store, "dev.test.reload").version == "1.1.0",
        Duration::from_secs(2)
    ));
    assert!(wait_phase(
        &store,
        "dev.test.reload",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let palette = store.palette_commands("reloaded", &CommandContext::default());
    assert!(
        palette
            .iter()
            .any(|(_, title)| title.contains("Reloaded Greet")),
        "{palette:?}"
    );
    // 声明了 menu = true 的命令只进右键菜单装饰点，不要求用户改配置。
    let menu = store.menu_commands();
    assert!(
        menu.iter()
            .any(|(id, title)| title == "context" && id.ends_with(".context")),
        "{menu:?}"
    );

    // 权限变化：reload 必须停止旧进程并要求重新确认，不能静默继承授权。
    std::fs::write(
        dir.join("plugin.toml"),
        manifest("dev.test.reload", "1.2.0", true, false, false),
    )
    .unwrap();
    store.reload_plugin("dev.test.reload").unwrap();
    assert!(wait_until(
        || store.phase("dev.test.reload").is_none(),
        Duration::from_secs(2)
    ));
    let entry_row = row_of(&store, "dev.test.reload");
    assert!(entry_row.enabled);
    assert!(entry_row.needs_approval);
    assert_eq!(entry_row.status, "permission confirmation required");
    match store.request_enable("dev.test.reload").unwrap() {
        EnableDecision::NeedsApproval { permissions } => {
            assert!(permissions.contains(&"terminal_context".to_string()));
        }
        EnableDecision::Approved => panic!("changed permissions must be re-confirmed"),
    }
    store.confirm_enable("dev.test.reload").unwrap();
    assert!(wait_phase(
        &store,
        "dev.test.reload",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clipboard_is_capability_gated_and_per_request_ask() {
    // 未声明 clipboard：直接 capability deny，不进入 ask。
    let root = tmpdir("clipboarddenied");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.clipdenied", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.clipdenied");
    assert!(wait_phase(
        &store,
        "dev.test.clipdenied",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.clipdenied.clipboard-read"));
    assert!(wait_activity(&store, "denied", Duration::from_secs(3)));
    assert!(store.pending_approvals().is_empty());
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);

    // 声明 clipboard：仍要每次 Allow。
    let root = tmpdir("clipboardallow");
    let body = manifest("dev.test.clipallow", "1.0.0", false, false, false)
        .replace("clipboard = false", "clipboard = true");
    install_plugin(&root, "plugin", &body, 0.0);
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.clipallow");
    assert!(wait_phase(
        &store,
        "dev.test.clipallow",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.clipallow.clipboard-read"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert_eq!(approval.method, "clipboard.read");
    assert!(store.resolve_host_request(
        "dev.test.clipallow",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "clip-data"})),
    ));
    assert!(wait_activity(
        &store,
        "CLIP:clip-data",
        Duration::from_secs(3)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disable_enable_restart_do_not_need_host_restart() {
    let root = tmpdir("lifecycle2");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.restart", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.restart");
    assert!(wait_phase(
        &store,
        "dev.test.restart",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let first = store
        .runtime_info("dev.test.restart")
        .and_then(|info| info.pid)
        .unwrap();
    store.restart_plugin("dev.test.restart").unwrap();
    assert!(wait_until(
        || store.phase("dev.test.restart") == Some(Phase::Running)
            && store
                .runtime_info("dev.test.restart")
                .and_then(|info| info.pid)
                .is_some_and(|pid| pid != first),
        Duration::from_secs(3)
    ));
    store.disable_plugin("dev.test.restart").unwrap();
    assert_eq!(row_of(&store, "dev.test.restart").status, "disabled");
    assert!(!store.spawn_command("plugin.dev.test.restart.greet"));
    // 已确认过权限，Enable 不需要重新确认。
    assert_eq!(
        store.request_enable("dev.test.restart").unwrap(),
        EnableDecision::Approved
    );
    assert!(wait_phase(
        &store,
        "dev.test.restart",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_v3_manifest_is_reported_and_never_executed() {
    let root = tmpdir("invalid");
    let dir = root.join("broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "api = 3\nhost_abi = 2\nid = \"dev.test.broken\"\nname = \"Broken\"\nversion = \"1\"\nruntime = \"process\"\nentry = \"main.py\"\n",
    )
    .unwrap();
    let marker = dir.join("ran");
    write_executable(
        &dir.join("main.py"),
        &format!("open({:?}, 'w').write('ran')\n", marker.to_string_lossy()),
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    store.start_process_plugins();
    std::thread::sleep(Duration::from_millis(100));
    assert!(!marker.exists());
    let entry_row = row_of(&store, "legacy:broken");
    assert!(!entry_row.supported);
    assert_eq!(entry_row.status, "invalid");
    assert!(entry_row.error.contains("host_abi"));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------- WASM 热路径

const WASM_MANIFEST: &str = r#"api = 3
host_abi = 3
id = "dev.xtervo.wasm-filter"
name = "WASM output filter test"
version = "1.0.0"
runtime = "wasm"
entry = "filter.wasm"
description = "real PTY output filter test"

[permissions.terminal]
output_filter = true
"#;

fn example_wasm() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/examples/plugin-wasm-filter/filter.wat");
    let text = std::fs::read_to_string(&path).expect("example WAT");
    wat::parse_str(&text).expect("example WAT assembles")
}

fn install_wasm_plugin(root: &Path, wasm: &[u8]) -> PathBuf {
    let dir = root.join("wasm-filter");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), WASM_MANIFEST).unwrap();
    std::fs::write(dir.join("filter.wasm"), wasm).unwrap();
    dir
}

fn frame_text(frame: &xtervo_core::Frame) -> String {
    let mut out = String::with_capacity(frame.rows * (frame.cols + 1));
    for row in 0..frame.rows {
        let line: String = frame.row(row).iter().map(|cell| cell.char()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn wait_frame_text(
    handle: &PaneHandle,
    mut frame: Box<xtervo_core::Frame>,
    needle: &str,
    timeout: Duration,
) -> (Box<xtervo_core::Frame>, String) {
    let deadline = Instant::now() + timeout;
    loop {
        frame = handle.pull(frame);
        let text = frame_text(&frame);
        if text.contains(needle) {
            return (frame, text);
        }
        if Instant::now() > deadline {
            panic!("等待 PTY 输出 {needle:?} 超时；当前屏幕:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn spawn_wasm_test_pane(filter: xtervo_core::OutputFilterHandle, script: &str) -> PaneHandle {
    xtervo_core::spawn(PaneConfig {
        cols: 100,
        rows: 24,
        scrollback: 200,
        shell: Some("/bin/sh".to_string()),
        args: vec!["-c".to_string(), script.to_string()],
        output_filter: Some(filter),
        ..PaneConfig::default()
    })
    .expect("spawn real PTY")
}

#[test]
fn wasm_first_enable_needs_confirmation_and_is_visible_in_settings() {
    let root = tmpdir("wasm-confirm");
    install_wasm_plugin(&root, &example_wasm());
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());

    match store.request_enable("dev.xtervo.wasm-filter").unwrap() {
        EnableDecision::NeedsApproval { permissions } => {
            assert_eq!(permissions, vec!["terminal.output_filter".to_string()]);
        }
        EnableDecision::Approved => panic!("WASM output_filter 首次启用必须确认"),
    }
    assert!(store.active_wasm_filters().is_empty());
    assert!(
        store.output_filter_handle().is_some(),
        "发现 wasm 插件即给 pane 句柄"
    );

    store.confirm_enable("dev.xtervo.wasm-filter").unwrap();
    let row = row_of(&store, "dev.xtervo.wasm-filter");
    assert_eq!(row.runtime, "wasm");
    assert_eq!(row.status, "running");
    assert_eq!(row.permissions, "terminal.output_filter");
    assert!(row.error.is_empty(), "running 不应带错误: {}", row.error);
    assert_eq!(
        store.active_wasm_filters(),
        vec!["dev.xtervo.wasm-filter".to_string()]
    );
    store.shutdown_processes();
    assert!(store.active_wasm_filters().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wasm_filter_transforms_real_pty_output_and_disable_restores_it() {
    let root = tmpdir("wasm-pty");
    install_wasm_plugin(&root, &example_wasm());
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.xtervo.wasm-filter");
    assert_eq!(
        store
            .rows()
            .iter()
            .find(|row| row.key == "dev.xtervo.wasm-filter")
            .unwrap()
            .status,
        "running"
    );
    let filter = store.output_filter_handle().expect("filter handle");
    let handle = spawn_wasm_test_pane(
        filter,
        "printf 'first [xtervo-wasm]\\n'; sleep 1.5; printf 'second [xtervo-wasm]\\n'; sleep 0.2",
    );
    let frame = handle.new_frame();
    let (frame, text) = wait_frame_text(
        &handle,
        frame,
        "first WASM-FILTER-OK",
        Duration::from_secs(5),
    );
    assert!(
        !text.contains("first [xtervo-wasm]"),
        "第一行应已被过滤:\n{text}"
    );
    assert!(
        store
            .wasm_filter_stats("dev.xtervo.wasm-filter")
            .unwrap()
            .calls
            > 0,
        "WASM 热路径必须被真实调用"
    );

    // 同一个 pane/同一份 PTY：禁用后下一行必须原样透传，不重建 pane。
    store.disable_plugin("dev.xtervo.wasm-filter").unwrap();
    assert!(store.active_wasm_filters().is_empty());
    let (_, text) = wait_frame_text(
        &handle,
        frame,
        "second [xtervo-wasm]",
        Duration::from_secs(5),
    );
    assert!(
        !text.contains("second WASM-FILTER-OK"),
        "禁用后不能再过滤:\n{text}"
    );
    assert_eq!(row_of(&store, "dev.xtervo.wasm-filter").status, "disabled");

    drop(handle);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wasm_enable_takes_effect_on_an_existing_pane() {
    let root = tmpdir("wasm-dynamic-enable");
    install_wasm_plugin(&root, &example_wasm());
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    // 插件已发现但默认 disabled：先创建 pane，验证 enable 不重建 pane。
    assert!(store.active_wasm_filters().is_empty());
    let filter = store.output_filter_handle().expect("filter handle");
    let handle = spawn_wasm_test_pane(
        filter,
        "printf 'before [xtervo-wasm]\n'; sleep 1.5; printf 'after [xtervo-wasm]\n'; sleep 0.2",
    );
    let frame = handle.new_frame();
    let (frame, text) = wait_frame_text(
        &handle,
        frame,
        "before [xtervo-wasm]",
        Duration::from_secs(5),
    );
    assert!(
        !text.contains("before WASM-FILTER-OK"),
        "启用前必须原样:\n{text}"
    );

    enable(&mut store, "dev.xtervo.wasm-filter");
    let (_, text) = wait_frame_text(
        &handle,
        frame,
        "after WASM-FILTER-OK",
        Duration::from_secs(5),
    );
    assert!(
        !text.contains("after [xtervo-wasm]"),
        "启用后必须过滤:\n{text}"
    );
    assert!(
        store
            .wasm_filter_stats("dev.xtervo.wasm-filter")
            .unwrap()
            .calls
            > 0
    );

    drop(handle);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wasm_trap_falls_back_to_original_bytes_in_real_pty() {
    let trap = r#"
(module
  (memory (export "memory") 8 256)
  (func (export "alloc") (param i32) (result i32) (i32.const 262144))
  (func (export "filter") (param i32) (param i32) (result i32) unreachable))
"#;
    let wasm = wat::parse_str(trap).unwrap();
    let root = tmpdir("wasm-trap-pty");
    install_wasm_plugin(&root, &wasm);
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.xtervo.wasm-filter");
    let filter = store.output_filter_handle().expect("filter handle");
    let handle = spawn_wasm_test_pane(filter, "printf 'fallback [xtervo-wasm]\\n'; sleep 0.3");
    let frame = handle.new_frame();
    let (_, text) = wait_frame_text(
        &handle,
        frame,
        "fallback [xtervo-wasm]",
        Duration::from_secs(5),
    );
    assert!(
        text.contains("fallback [xtervo-wasm]"),
        "必须原样放行:\n{text}"
    );
    let stats = store
        .wasm_filter_stats("dev.xtervo.wasm-filter")
        .expect("stats");
    assert_eq!(stats.failures, 1, "trap 只应发生一次（随后熔断）");
    let row = row_of(&store, "dev.xtervo.wasm-filter");
    assert_eq!(row.status, "failed");
    assert!(!row.error.is_empty(), "设置页必须显示陷阱原因");
    assert!(
        row.error.contains("unreachable") || row.error.contains("陷阱"),
        "{}",
        row.error
    );

    drop(handle);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn wasm_reload_and_broken_module_error_are_reported_without_crash() {
    let root = tmpdir("wasm-reload");
    let dir = install_wasm_plugin(&root, &example_wasm());
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.xtervo.wasm-filter");

    store
        .reload_plugin("dev.xtervo.wasm-filter")
        .expect("valid reload");
    assert!(store
        .active_wasm_filters()
        .contains(&"dev.xtervo.wasm-filter".to_string()));
    assert_eq!(row_of(&store, "dev.xtervo.wasm-filter").status, "running");

    std::fs::write(dir.join("filter.wasm"), b"not a wasm module").unwrap();
    let err = store
        .reload_plugin("dev.xtervo.wasm-filter")
        .expect_err("broken module must fail reload");
    assert!(err.contains("WASM"), "{err}");
    assert!(
        store.active_wasm_filters().is_empty(),
        "坏模块必须停止旧过滤器"
    );
    let row = row_of(&store, "dev.xtervo.wasm-filter");
    assert_eq!(row.status, "failed");
    assert!(row.error.contains("WASM"), "{}", row.error);

    // 修好后 Restart（不重启 xtervo）恢复。
    std::fs::write(dir.join("filter.wasm"), example_wasm()).unwrap();
    store
        .restart_plugin("dev.xtervo.wasm-filter")
        .expect("restart fixed module");
    assert_eq!(row_of(&store, "dev.xtervo.wasm-filter").status, "running");
    assert!(store
        .active_wasm_filters()
        .contains(&"dev.xtervo.wasm-filter".to_string()));

    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn window_id_routes_commands_reads_surfaces_status_and_cleanup() {
    let root = tmpdir("window-routing");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.window", "1.0.0", true, false, true),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.window");
    assert!(wait_phase(
        &store,
        "dev.test.window",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let pid = store
        .runtime_info("dev.test.window")
        .and_then(|info| info.pid)
        .expect("plugin pid");
    store.set_active_window(Some(1));
    store.set_known_windows(&[1, 2]);

    // ---- window 1: command.invoke.window_id -> terminal.read ask 记录窗口
    assert!(store.spawn_command_for("plugin.dev.test.window.terminal-read-window", Some(1)));
    assert!(wait_until(
        || store
            .pending_approvals_for(1, Some(1))
            .iter()
            .any(|approval| approval.window_id == Some(1)),
        Duration::from_secs(3)
    ));
    assert!(
        store.pending_approvals_for(2, Some(1)).is_empty(),
        "window 2 must not see window 1's ask"
    );
    let approval = store.pending_approvals_for(1, Some(1)).remove(0);
    assert_eq!(approval.method, "terminal.read");
    assert!(store.resolve_host_request(
        "dev.test.window",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "tail-A"})),
    ));
    assert!(wait_until(
        || store
            .take_notices_for(1, Some(1))
            .iter()
            .any(|notice| notice.text.contains("READWIN[1]:tail-A")),
        Duration::from_secs(3)
    ));
    assert!(
        store.take_notices_for(2, Some(1)).is_empty(),
        "window 1 notification must not leak into window 2"
    );

    // ---- window 2: 同一插件进程，显式 window_id 路由到另一个窗口
    assert!(store.spawn_command_for("plugin.dev.test.window.terminal-read-window", Some(2)));
    assert!(wait_until(
        || store
            .pending_approvals_for(2, Some(1))
            .iter()
            .any(|approval| approval.window_id == Some(2)),
        Duration::from_secs(3)
    ));
    assert!(store.pending_approvals_for(1, Some(1)).is_empty());
    let approval = store.pending_approvals_for(2, Some(1)).remove(0);
    assert!(store.resolve_host_request(
        "dev.test.window",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "tail-B"})),
    ));
    assert!(wait_until(
        || store
            .take_notices_for(2, Some(1))
            .iter()
            .any(|notice| notice.text.contains("READWIN[2]:tail-B")),
        Duration::from_secs(3)
    ));
    assert!(store.take_notices_for(1, Some(1)).is_empty());
    assert_eq!(
        store.runtime_info("dev.test.window").unwrap().pid,
        Some(pid),
        "two windows must share one plugin process"
    );

    // ---- 宿主请求 surface：只回到请求窗口
    store
        .request_surface_for(
            "dev.test.window",
            "panel",
            Some("main".to_string()),
            Some(1),
        )
        .unwrap();
    let window1_surfaces = wait_for_surfaces(&store, 1, Some(1));
    assert!(
        window1_surfaces
            .iter()
            .any(|surface| { surface.window_id == Some(1) && surface.surface == "panel:main" }),
        "{window1_surfaces:?}"
    );
    assert!(store.take_surfaces_for(2, Some(1)).is_empty());

    // ---- 插件主动 ui.render：显式 window_id=2，不覆盖 window 1
    assert!(store.spawn_command_for("plugin.dev.test.window.panel-window", Some(2)));
    let window2_surfaces = wait_for_surfaces(&store, 2, Some(1));
    let pushed = window2_surfaces
        .iter()
        .find(|surface| surface.window_id == Some(2))
        .expect("pushed surface for window 2");
    assert!(pushed.pushed, "plugin push must be marked pushed");
    assert_eq!(pushed.doc.title, "PANEL[2]");
    assert!(store.take_surfaces_for(1, Some(1)).is_empty());

    // ---- ui.status 按窗口；status.get 的全局状态仍对两个窗口可见
    assert!(store.spawn_command_for("plugin.dev.test.window.status-window", Some(1)));
    assert!(wait_until(
        || store
            .status_snapshot_for_window(1)
            .1
            .iter()
            .any(|status| status.text == "status-window-1"),
        Duration::from_secs(3)
    ));
    assert!(
        !store
            .status_snapshot_for_window(2)
            .1
            .iter()
            .any(|status| status.text == "status-window-1"),
        "window 1 status must not appear in window 2"
    );
    assert!(store
        .status_snapshot_for_window(2)
        .1
        .iter()
        .any(|status| status.text == "status-ready"));

    // ---- 关闭窗口 2：清理它的 surface/status/ask
    assert!(store.spawn_command_for("plugin.dev.test.window.panel-window", Some(2)));
    let _ = wait_for_surfaces(&store, 2, Some(1));
    assert!(store.spawn_command_for("plugin.dev.test.window.terminal-read-window", Some(2)));
    assert!(wait_until(
        || !store.pending_approvals_for(2, Some(1)).is_empty(),
        Duration::from_secs(3)
    ));
    let cancelled = store.forget_window(2);
    assert_eq!(
        cancelled, 1,
        "pending ask for the closed window is cancelled"
    );
    assert!(store.pending_approvals_for(2, Some(1)).is_empty());
    assert!(store.take_surfaces_for(2, Some(1)).is_empty());
    assert!(!store
        .status_snapshot_for_window(2)
        .1
        .iter()
        .any(|status| status.text.starts_with("status-window-")));
    // 全局 status.get 不受窗口关闭影响
    assert!(store
        .status_snapshot_for_window(1)
        .1
        .iter()
        .any(|status| status.text.starts_with("status-window-1")));
    assert_eq!(store.phase("dev.test.window"), Some(Phase::Running));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
fn wait_for_surfaces(
    store: &PluginStore,
    window_id: u64,
    active: Option<u64>,
) -> Vec<SurfaceUpdate> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        all.extend(store.take_surfaces_for(window_id, active));
        if !all.is_empty() {
            return all;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    all
}

#[test]
fn window_target_matching_is_exact_for_explicit_ids() {
    assert!(window_matches_target(Some(2), 2, Some(1)));
    assert!(!window_matches_target(Some(2), 1, Some(1)));
    assert!(window_matches_target(None, 1, Some(1)));
    assert!(!window_matches_target(None, 2, Some(1)));
}

#[cfg(unix)]
#[test]
fn concurrent_asks_route_back_to_their_own_window() {
    let root = tmpdir("window-asks");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.asks", "1.0.0", true, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.asks");
    assert!(wait_phase(
        &store,
        "dev.test.asks",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.set_active_window(Some(1));
    store.set_known_windows(&[1, 2]);
    assert!(store.spawn_command_for("plugin.dev.test.asks.terminal-read-window", Some(1)));
    assert!(store.spawn_command_for("plugin.dev.test.asks.terminal-read-window", Some(2)));
    assert!(wait_until(
        || {
            !store.pending_approvals_for(1, Some(1)).is_empty()
                && !store.pending_approvals_for(2, Some(1)).is_empty()
        },
        Duration::from_secs(3)
    ));
    assert_eq!(store.pending_approvals().len(), 2);
    let ask1 = store.pending_approvals_for(1, Some(1)).remove(0);
    let ask2 = store.pending_approvals_for(2, Some(1)).remove(0);
    assert_eq!(ask1.window_id, Some(1));
    assert_eq!(ask2.window_id, Some(2));
    assert_ne!(ask1.request_id, ask2.request_id);
    assert_eq!(
        store.pending_approvals_for(1, Some(1))[0].request_id,
        ask1.request_id
    );

    // 先回复 window 2，再回复 window 1：回复只回到各自请求。
    assert!(store.resolve_host_request(
        "dev.test.asks",
        ask2.request_id,
        HostOutcome::Ok(json!({"text": "tail-two"})),
    ));
    assert!(wait_until(
        || store
            .take_notices_for(2, Some(1))
            .iter()
            .any(|notice| notice.text.contains("READWIN[2]:tail-two")),
        Duration::from_secs(3)
    ));
    assert!(store.take_notices_for(1, Some(1)).is_empty());
    assert!(store.resolve_host_request(
        "dev.test.asks",
        ask1.request_id,
        HostOutcome::Ok(json!({"text": "tail-one"})),
    ));
    assert!(wait_until(
        || store
            .take_notices_for(1, Some(1))
            .iter()
            .any(|notice| notice.text.contains("READWIN[1]:tail-one")),
        Duration::from_secs(3)
    ));
    assert!(store.pending_approvals().is_empty());
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------- P1/P2 回归测试

/// 测试进程退出时兜底杀掉 sleep 中的假插件，避免失败用例留下孤儿进程。
#[cfg(unix)]
struct KillOnDrop(Option<u32>);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // SAFETY: pid 来自 child.id()，只会向该进程组发 SIGKILL。
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    // SAFETY: signal 0 只做存在性检查，不递送信号。
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// [P1] 插件停读 stdin 时，宿主写线程不能阻塞 supervisor：命令按 deadline 超时，
/// shutdown 有界返回，旧子进程被回收。
#[cfg(unix)]
#[test]
fn frozen_stdin_command_times_out_and_shutdown_kills_process() {
    let root = tmpdir("stdin-freeze");
    let dir = install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.frozen", "1.0.0", false, false, false),
        0.0,
    );
    let script = std::fs::read_to_string(dir.join("main.py"))
        .unwrap()
        .replace("FREEZE_AFTER_INIT = False", "FREEZE_AFTER_INIT = True");
    write_executable(&dir.join("main.py"), &script);
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.command_timeout = Duration::from_millis(300);
    options.ping_interval = Duration::from_secs(60);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.frozen");
    assert!(wait_phase(
        &store,
        "dev.test.frozen",
        Phase::Running,
        Duration::from_secs(3)
    ));
    let pid = store
        .runtime_info("dev.test.frozen")
        .and_then(|info| info.pid)
        .expect("plugin pid");
    let _guard = KillOnDrop(Some(pid));

    let handle = store
        .supervisors
        .get("dev.test.frozen")
        .expect("supervisor handle");
    // 先塞一条 200 KiB 事件填满管道（子进程停读后 write(2) 会阻塞），
    // 再发一条 200 KiB 命令：修复前 supervisor 卡在事件 write_all 上，
    // 命令永远等不到 deadline；修复后命令在 300ms 后超时。
    let blob = "y".repeat(200 * 1024);
    assert!(handle
        .tx
        .send(SupervisorMsg::Event {
            event: PluginEvent::Startup,
            payload: json!({ "blob": blob }),
        })
        .is_ok());
    let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
    assert!(handle
        .tx
        .send(SupervisorMsg::Command {
            command: "greet".to_string(),
            args: vec!["x".repeat(200 * 1024)],
            window_id: None,
            reply: reply_tx,
        })
        .is_ok());
    let outcome = reply_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("停读 stdin 时命令必须按 deadline 返回，而不是让 supervisor 阻塞");
    assert_eq!(
        outcome,
        CommandOutcome::Timeout,
        "停读 stdin 应表现为命令 deadline 超时"
    );
    assert!(
        wait_until(|| process_gone(pid), Duration::from_secs(2)),
        "命令超时后旧插件进程必须被回收（pid={pid}）"
    );

    let started = Instant::now();
    store.shutdown_processes();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "停读 stdin 时 shutdown 必须 ≤2s，实际 {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// [P1] 插件伪造 window_id 的 ask 必须立即拒绝，不能占满审批队列或阻止合法 ask。
#[cfg(unix)]
#[test]
fn fake_window_asks_do_not_occupy_approval_slots() {
    let root = tmpdir("fake-ask-window");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.fakeask", "1.0.0", true, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.fakeask");
    assert!(wait_phase(
        &store,
        "dev.test.fakeask",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.set_active_window(Some(1));

    assert!(store.spawn_command("plugin.dev.test.fakeask.terminal-read-fake-windows"));
    // 并行测试会争抢进程/FD，sidecar 可能重启一次；给首次活动更长的窗口。
    assert!(wait_activity(
        &store,
        "fake-windows-sent",
        Duration::from_secs(15)
    ));
    std::thread::sleep(Duration::from_millis(100));
    let approvals = store.pending_approvals();
    assert!(
        approvals.iter().all(|approval| !matches!(
            approval.window_id,
            Some(window) if (1000..=1019).contains(&window)
        )),
        "伪造窗口的 ask 不应进入审批队列: {approvals:?}"
    );
    assert!(
        store.pending_approvals().is_empty(),
        "20 个伪造窗口 ask 必须全部立即拒绝，不能占审批位"
    );

    // 随后的合法 ask 仍必须能排队、能答复。
    assert!(store.spawn_command_for("plugin.dev.test.fakeask.terminal-read-window", Some(1)));
    assert!(wait_until(
        || !store.pending_approvals_for(1, Some(1)).is_empty(),
        Duration::from_secs(3)
    ));
    assert_eq!(
        store.pending_approvals().len(),
        1,
        "只应有合法窗口的 ask: {:?}",
        store.pending_approvals()
    );
    let approval = store.pending_approvals_for(1, Some(1)).remove(0);
    assert!(store.resolve_host_request(
        "dev.test.fakeask",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "legal-tail"})),
    ));
    assert!(wait_until(
        || store
            .take_notices_for(1, Some(1))
            .iter()
            .any(|notice| notice.text.contains("READWIN[1]:legal-tail")),
        Duration::from_secs(3)
    ));
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

/// [P1] 插件自选 window_id 不能无界增长 status map：未知窗口拒绝、每插件最多 2 个
/// 窗口专属状态、UI 快照不 clone 全表。
#[cfg(unix)]
#[test]
fn fake_window_status_ids_do_not_grow_status_map() {
    let root = tmpdir("fake-status-window");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.fakestatus", "1.0.0", false, false, true),
        0.0,
    );
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.test.fakestatus");
    assert!(wait_phase(
        &store,
        "dev.test.fakestatus",
        Phase::Running,
        Duration::from_secs(3)
    ));
    store.set_active_window(Some(1));

    assert!(store.spawn_command("plugin.dev.test.fakestatus.spam-fake-status"));
    assert!(wait_activity(
        &store,
        "spam-status-sent",
        Duration::from_secs(10)
    ));

    let statuses = store.shared.statuses.lock().expect("statuses");
    let from_this_plugin: Vec<(Option<u64>, String)> = statuses
        .iter()
        .filter(|((plugin, _), _)| plugin == "dev.test.fakestatus")
        .map(|((_, window), status)| (*window, status.text.clone()))
        .collect();
    assert!(
        from_this_plugin
            .iter()
            .all(|(window, _)| window.is_none() || *window == Some(1)),
        "未知窗口 status 必须被拒绝，实际 key: {from_this_plugin:?}"
    );
    let explicit = from_this_plugin
        .iter()
        .filter(|(window, _)| window.is_some())
        .count();
    assert!(
        explicit <= 2,
        "每插件窗口专属 status 上限 2，实际 {explicit}"
    );
    assert!(
        statuses.len() <= 3,
        "10k 伪造窗口后 status map 必须有界，实际 {}",
        statuses.len()
    );
    drop(statuses);

    // status 轮询的全局状态仍然可见，窗口快照也不复制全表。
    assert!(wait_until(
        || store
            .status_snapshot_for_window(1)
            .1
            .iter()
            .any(|status| status.text == "status-ready"),
        Duration::from_secs(3)
    ));

    // 真实窗口也不能让单插件的窗口专属 status 超过 2 条：按窗口 id 升序驱逐。
    store.set_known_windows(&[1, 2, 3, 4]);
    store.set_active_window(Some(1));
    for window in [1u64, 2, 3, 4] {
        assert!(store.spawn_command_for("plugin.dev.test.fakestatus.status-window", Some(window)));
    }
    assert!(wait_until(
        || store.shared.statuses.lock().is_ok_and(|statuses| {
            statuses
                .keys()
                .any(|(plugin, window)| plugin == "dev.test.fakestatus" && *window == Some(4))
        }),
        Duration::from_secs(3)
    ));
    let statuses = store.shared.statuses.lock().expect("statuses");
    let mut explicit: Vec<u64> = statuses
        .keys()
        .filter(|(plugin, window)| plugin == "dev.test.fakestatus" && window.is_some())
        .filter_map(|(_, window)| *window)
        .collect();
    explicit.sort_unstable();
    assert_eq!(
        explicit,
        vec![3, 4],
        "每插件最多 2 个窗口专属 status，最旧（最小 id）先驱逐"
    );
    drop(statuses);
    assert!(store.known_windows() == vec![1, 2, 3, 4]);
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

/// [P2] 插件 stderr 里的 ESC/CR 必须在进入 UI/日志前被清理。
#[cfg(unix)]
#[test]
fn plugin_stderr_control_bytes_are_stripped_from_ui() {
    let root = tmpdir("stderr-sanitize");
    install_plugin(
        &root,
        "plugin",
        &manifest("dev.test.stderr", "1.0.0", false, false, false),
        0.0,
    );
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.max_restarts = 0;
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.stderr");
    assert!(wait_phase(
        &store,
        "dev.test.stderr",
        Phase::Running,
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.test.stderr.stderr-crash"));
    assert!(wait_phase(
        &store,
        "dev.test.stderr",
        Phase::Failed,
        Duration::from_secs(5)
    ));
    let info = store.runtime_info("dev.test.stderr").expect("runtime");
    assert!(
        info.error.contains("red-alert"),
        "stderr 文本应保留可见内容: {}",
        info.error
    );
    assert!(
        !info.error.contains('\x1b')
            && !info.error.contains('\r')
            && !info.error.contains("[31m")
            && !info.error.contains("[2J"),
        "stderr 的 ESC/CR/ANSI 不能原样进入 runtime error: {:?}",
        info.error
    );
    let row = row_of(&store, "dev.test.stderr");
    assert!(
        !row.error.contains('\x1b') && !row.error.contains('\r') && !row.error.contains("[31m"),
        "stderr 的 ESC/CR/ANSI 不能原样进入设置页: {:?}",
        row.error
    );
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}

/// [P2] Enable 读取 WASM 时必须先 stat：发现后文件被放大/不可读也不能整读。
#[cfg(unix)]
#[test]
fn oversized_wasm_is_rejected_from_metadata_without_reading() {
    let root = tmpdir("wasm-oversize");
    // 先在发现阶段放一个合法小模块，再在 Enable 前把文件放大并设为不可读。
    let dir = install_wasm_plugin(&root, &example_wasm());
    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    let wasm_path = dir.join("filter.wasm");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&wasm_path)
        .unwrap();
    file.set_len((crate::plugin_wasm::MAX_WASM_BYTES + 1) as u64)
        .unwrap();
    drop(file);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wasm_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    let error = store
        .confirm_enable("dev.xtervo.wasm-filter")
        .expect_err("超限 WASM 必须被拒绝");
    let _ = std::fs::set_permissions(&wasm_path, std::fs::Permissions::from_mode(0o600));
    assert!(
        error.contains("过大") || error.contains("上限"),
        "应按文件大小直接拒绝，而不是读取失败: {error}"
    );
    assert!(
        !error.contains("Permission denied"),
        "超限文件不应被打开读取: {error}"
    );
    assert!(store.active_wasm_filters().is_empty());
    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}
