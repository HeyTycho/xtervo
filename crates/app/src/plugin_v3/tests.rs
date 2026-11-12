// SPDX-License-Identifier: GPL-3.0-only
//! Plugin API v3 宿主测试。
//!
//! 所有集成测试都使用真实 Python 子进程 + JSON-RPC 2.0 stdio，不 mock supervisor。
//! 测试超时被缩短（[`SupervisorOptions`]），生产默认值不变。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use tervo_core::{PaneConfig, PaneHandle};

use super::manifest::{HOST_ABI, PLUGIN_API_VERSION};
use super::*;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir(tag: &str) -> PathBuf {
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "tervo-plugin-v3-{}-{tag}-{unique}",
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
    "terminal-write",
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
        .map(|command| format!("[[commands]]\nid = \"{command}\"\ntitle = \"{command}\"\n"))
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
IGNORE_PING = False
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
    kind, command_id = pending
    if "error" in msg:
        error = msg["error"]
        respond(command_id, {"ok": False, "message": "%s: %s" % (kind, error.get("message", "error"))})
    else:
        result = msg.get("result") or {}
        if kind == "terminal.read":
            respond(command_id, {"ok": True, "message": "READ:" + str(result.get("text", ""))})
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
        respond(rid, {"protocol_version": PROTOCOL})
    elif method == "initialized":
        pass
    elif method == "command.invoke":
        command = params.get("command")
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
            _pending[pending] = ("terminal.read", rid)
        elif command == "terminal-write":
            pending = request("terminal.write", {"text": "echo hello\n"})
            _pending[pending] = ("terminal.write", rid)
        elif command == "clipboard-read":
            pending = request("clipboard.read", {})
            _pending[pending] = ("clipboard.read", rid)
        elif command == "context":
            pending = request("context.version", {})
            _pending[pending] = ("context.version", rid)
        elif command == "notify":
            send({"jsonrpc": "2.0", "method": "ui.notify", "params": {"text": "plugin-notify", "level": "info"}})
            respond(rid, {"ok": True, "message": "notified"})
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
            _pending[pending] = ("ui.render", rid)
        elif command == "credentials":
            pending = request("credentials.get", {})
            _pending[pending] = ("credentials.get", rid)
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

fn install_plugin(
    root: &Path,
    dir_name: &str,
    manifest_body: &str,
    status_delay: f32,
) -> PathBuf {
    let dir = root.join(dir_name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), manifest_body).unwrap();
    let script = PLUGIN_SCRIPT.replace("__STATUS_DELAY__", &status_delay.to_string());
    write_executable(&dir.join("main.py"), &script);
    dir
}

fn load(root: &Path, state: &Path, options: SupervisorOptions) -> PluginStore {
    PluginStore::load_with_options(root, state, options)
}

fn enable(store: &mut PluginStore, key: &str) {
    match store.request_enable(key).unwrap() {
        EnableDecision::Approved => {},
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
    let mut permissions = std::fs::metadata(dest.join("main.py")).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(dest.join("main.py"), permissions).unwrap();

    let state = root.join("state.toml");
    let mut store = load(&root, &state, fast_options());
    enable(&mut store, "dev.tervo.echo");
    assert!(wait_phase(&store, "dev.tervo.echo", Phase::Running, Duration::from_secs(3)));
    assert!(store.spawn_command("plugin.dev.tervo.echo.greet"));
    assert!(wait_activity(&store, "Hello from Echo Plugin API v3", Duration::from_secs(3)));
    assert!(wait_until(
        || store
            .status_snapshot()
            .1
            .iter()
            .any(|status| status.text == "Echo v3 ready"),
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.tervo.echo.notify"));
    assert!(wait_until(
        || store
            .take_notices()
            .iter()
            .any(|notice| notice.text.contains("Echo says hi")),
        Duration::from_secs(3)
    ));
    store.request_surface("dev.tervo.echo", "settings", None).unwrap();
    assert!(wait_until(
        || {
            store
                .take_surfaces()
                .iter()
                .any(|surface| surface.surface == "settings")
        },
        Duration::from_secs(3)
    ));
    assert!(store.spawn_command("plugin.dev.tervo.echo.read-terminal"));
    assert!(wait_until(
        || !store.pending_approvals().is_empty(),
        Duration::from_secs(3)
    ));
    let approval = store.pending_approvals().remove(0);
    assert!(store.resolve_host_request(
        "dev.tervo.echo",
        approval.request_id,
        HostOutcome::Ok(json!({"text": "example-tail"})),
    ));
    assert!(wait_activity(&store, "example-tail", Duration::from_secs(3)));
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
        },
        EnableDecision::Approved => panic!("first enable must ask for confirmation"),
    }
    assert!(!state_path.exists(), "a pending confirmation must not enable");
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
    assert!(wait_phase(&store, "dev.test.lifecycle", Phase::Running, Duration::from_secs(3)));

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
    assert_eq!(status.command.as_deref(), Some("plugin.dev.test.lifecycle.greet"));

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
        || store.phase("dev.test.lifecycle").is_none_or(|phase| phase == Phase::Stopped),
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
    assert!(wait_phase(&store, "dev.test.badjson", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.timeout", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.pingok", Phase::Running, Duration::from_secs(3)));
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
    let script = std::fs::read_to_string(dir.join("main.py")).unwrap().replace(
        "IGNORE_PING = False",
        "IGNORE_PING = True",
    );
    write_executable(&dir.join("main.py"), &script);
    let state = root.join("state.toml");
    let mut options = fast_options();
    options.ping_interval = Duration::from_millis(50);
    options.ping_timeout = Duration::from_millis(80);
    let mut store = load(&root, &state, options);
    enable(&mut store, "dev.test.pingfail");
    assert!(wait_phase(&store, "dev.test.pingfail", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.line", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.output", Phase::Running, Duration::from_secs(3)));
    assert!(store.spawn_command("plugin.dev.test.output.longline"));
    assert!(wait_phase(&store, "dev.test.output", Phase::Failed, Duration::from_secs(4)));
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
    assert!(wait_phase(&store, "dev.test.terminal", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_activity(&store, "READ:tail-data", Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.noperm", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.write", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_activity(&store, "terminal-write", Duration::from_secs(3)));

    assert!(store.spawn_command("plugin.dev.test.write.credentials"));
    assert!(wait_activity(&store, "credentials.get", Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.ui", Phase::Running, Duration::from_secs(3)));

    assert!(store.spawn_command("plugin.dev.test.ui.notify"));
    assert!(wait_until(
        || store
            .take_notices()
            .iter()
            .any(|notice| notice.text.contains("plugin-notify")),
        Duration::from_secs(3)
    ));

    store.request_surface("dev.test.ui", "settings", None).unwrap();
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
    assert!(wait_phase(&store, "dev.test.reload", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.reload", Phase::Running, Duration::from_secs(3)));
    let palette = store.palette_commands("reloaded");
    assert!(
        palette
            .iter()
            .any(|(_, title)| title.contains("Reloaded Greet")),
        "{palette:?}"
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
        },
        EnableDecision::Approved => panic!("changed permissions must be re-confirmed"),
    }
    store.confirm_enable("dev.test.reload").unwrap();
    assert!(wait_phase(&store, "dev.test.reload", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.clipdenied", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.clipallow", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_activity(&store, "CLIP:clip-data", Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.restart", Phase::Running, Duration::from_secs(3)));
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
    assert!(wait_phase(&store, "dev.test.restart", Phase::Running, Duration::from_secs(3)));
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
id = "dev.tervo.wasm-filter"
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

fn frame_text(frame: &tervo_core::Frame) -> String {
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
    mut frame: Box<tervo_core::Frame>,
    needle: &str,
    timeout: Duration,
) -> (Box<tervo_core::Frame>, String) {
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

fn spawn_wasm_test_pane(filter: tervo_core::OutputFilterHandle, script: &str) -> PaneHandle {
    tervo_core::spawn(PaneConfig {
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

    match store.request_enable("dev.tervo.wasm-filter").unwrap() {
        EnableDecision::NeedsApproval { permissions } => {
            assert_eq!(permissions, vec!["terminal.output_filter".to_string()]);
        },
        EnableDecision::Approved => panic!("WASM output_filter 首次启用必须确认"),
    }
    assert!(store.active_wasm_filters().is_empty());
    assert!(store.output_filter_handle().is_some(), "发现 wasm 插件即给 pane 句柄");

    store.confirm_enable("dev.tervo.wasm-filter").unwrap();
    let row = row_of(&store, "dev.tervo.wasm-filter");
    assert_eq!(row.runtime, "wasm");
    assert_eq!(row.status, "running");
    assert_eq!(row.permissions, "terminal.output_filter");
    assert!(row.error.is_empty(), "running 不应带错误: {}", row.error);
    assert_eq!(
        store.active_wasm_filters(),
        vec!["dev.tervo.wasm-filter".to_string()]
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
    enable(&mut store, "dev.tervo.wasm-filter");
    assert_eq!(
        store.rows().iter().find(|row| row.key == "dev.tervo.wasm-filter").unwrap().status,
        "running"
    );
    let filter = store.output_filter_handle().expect("filter handle");
    let handle = spawn_wasm_test_pane(
        filter,
        "printf 'first [tervo-wasm]\\n'; sleep 1.5; printf 'second [tervo-wasm]\\n'; sleep 0.2",
    );
    let frame = handle.new_frame();
    let (frame, text) = wait_frame_text(
        &handle,
        frame,
        "first WASM-FILTER-OK",
        Duration::from_secs(5),
    );
    assert!(!text.contains("first [tervo-wasm]"), "第一行应已被过滤:\n{text}");
    assert!(
        store.wasm_filter_stats("dev.tervo.wasm-filter").unwrap().calls > 0,
        "WASM 热路径必须被真实调用"
    );

    // 同一个 pane/同一份 PTY：禁用后下一行必须原样透传，不重建 pane。
    store.disable_plugin("dev.tervo.wasm-filter").unwrap();
    assert!(store.active_wasm_filters().is_empty());
    let (_, text) = wait_frame_text(
        &handle,
        frame,
        "second [tervo-wasm]",
        Duration::from_secs(5),
    );
    assert!(
        !text.contains("second WASM-FILTER-OK"),
        "禁用后不能再过滤:\n{text}"
    );
    assert_eq!(
        row_of(&store, "dev.tervo.wasm-filter").status,
        "disabled"
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
    enable(&mut store, "dev.tervo.wasm-filter");
    let filter = store.output_filter_handle().expect("filter handle");
    let handle = spawn_wasm_test_pane(
        filter,
        "printf 'fallback [tervo-wasm]\\n'; sleep 0.3",
    );
    let frame = handle.new_frame();
    let (_, text) = wait_frame_text(
        &handle,
        frame,
        "fallback [tervo-wasm]",
        Duration::from_secs(5),
    );
    assert!(text.contains("fallback [tervo-wasm]"), "必须原样放行:\n{text}");
    let stats = store.wasm_filter_stats("dev.tervo.wasm-filter").expect("stats");
    assert_eq!(stats.failures, 1, "trap 只应发生一次（随后熔断）");
    let row = row_of(&store, "dev.tervo.wasm-filter");
    assert_eq!(row.status, "failed");
    assert!(!row.error.is_empty(), "设置页必须显示陷阱原因");
    assert!(row.error.contains("unreachable") || row.error.contains("陷阱"), "{}", row.error);

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
    enable(&mut store, "dev.tervo.wasm-filter");

    store.reload_plugin("dev.tervo.wasm-filter").expect("valid reload");
    assert!(store.active_wasm_filters().contains(&"dev.tervo.wasm-filter".to_string()));
    assert_eq!(row_of(&store, "dev.tervo.wasm-filter").status, "running");

    std::fs::write(dir.join("filter.wasm"), b"not a wasm module").unwrap();
    let err = store
        .reload_plugin("dev.tervo.wasm-filter")
        .expect_err("broken module must fail reload");
    assert!(err.contains("WASM"), "{err}");
    assert!(store.active_wasm_filters().is_empty(), "坏模块必须停止旧过滤器");
    let row = row_of(&store, "dev.tervo.wasm-filter");
    assert_eq!(row.status, "failed");
    assert!(row.error.contains("WASM"), "{}", row.error);

    // 修好后 Restart（不重启 Tervo）恢复。
    std::fs::write(dir.join("filter.wasm"), example_wasm()).unwrap();
    store.restart_plugin("dev.tervo.wasm-filter").expect("restart fixed module");
    assert_eq!(row_of(&store, "dev.tervo.wasm-filter").status, "running");
    assert!(store.active_wasm_filters().contains(&"dev.tervo.wasm-filter".to_string()));

    store.shutdown_processes();
    let _ = std::fs::remove_dir_all(&root);
}
