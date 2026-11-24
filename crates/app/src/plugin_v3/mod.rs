// SPDX-License-Identifier: GPL-3.0-only
//! xtervo Plugin API v3 宿主（唯一受支持的插件协议）。
//!
//! 设计约束（与 `docs/PLUGIN_V3.md` 一致）：
//!
//! - **只加载 `api = 3` / `host_abi = 3`**。v1/v2 清单不会被执行，只在设置页
//!   显示 `unsupported` 与迁移提示；没有双协议兼容层。
//! - 每个插件是**独立子进程**，JSON-RPC 2.0 line-delimited stdio；宿主负责
//!   spawn/握手/超时/重启/关闭，任何插件故障都不影响终端。
//! - 权限 = **capability gate + 用户确认**，不是 OS 沙箱。宿主只 gate 自己
//!   代理的 API（terminal.read/write、clipboard），物理上插件仍拥有当前用户
//!   的系统权限。
//! - **MCP 是 xtervo 内嵌功能，不是插件能力**：这里没有、也不会实现插件 MCP
//!   server/代理。插件只能声明固定贡献点（commands/status/events/theme/
//!   panels/settings/assistant 占位）。
//! - 所有 UI 由宿主渲染。插件只能提供声明式文档，不能注入 Slint/WASM/HTML。
//!
//! 该模块取代旧 `plugin.rs`（v1/v2，保留为未编译 legacy 文件），不保留旧协议
//! 代码路径。

mod cli;
pub mod context;
pub mod install;
mod manifest;
mod protocol;
pub mod signature;
mod state;
mod supervisor;
#[cfg(test)]
mod install_tests;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use xtervo_core::{OutputFilterHandle, WakeSignal};

use crate::plugin_wasm::{
    WasmFilterRuntime, WasmFilterStatsSnapshot, WasmOutputHost,
};

pub use cli::run_cli;
pub use context::{CommandContext, WhenExpr};
pub use install::{
    install_package, rollback_package, InstallOptions, InstallReport, RollbackOptions,
    RollbackReport,
};
pub use manifest::{CommandDecl, Manifest, PluginEvent, PLUGIN_API_VERSION};
pub use protocol::{
    render_markdown_plain, StatusPayload, SurfaceDoc, SurfaceElement, MAX_CLIPBOARD_CHARS,
};
pub use state::{load_state, save_state, PersistedState};
pub use supervisor::{
    spawn_supervisor, CommandOutcome, HostOutcome, Phase, PluginRuntimeConfig, RuntimeInfo,
    SupervisorHandle, SupervisorMsg, SupervisorOptions, SupervisorShared,
};

use protocol::clip_chars;

pub const ACTIVITY_CAPACITY: usize = 8;
pub const MAX_ACTIVITY_SUMMARY_CHARS: usize = 100;
const MAX_ERROR_TEXT_CHARS: usize = 800;
const MAX_PLUGIN_MANIFEST_READ: u64 = manifest::MAX_MANIFEST_BYTES as u64;

/// 最近插件活动（只保留摘要；完整输出只在日志里）。
#[derive(Debug, Default)]
pub struct ActivityLog {
    entries: VecDeque<(u64, String)>,
    revision: u64,
    wake: Option<Arc<WakeSignal>>,
}

impl ActivityLog {
    pub fn push(&mut self, line: String) {
        self.entries.push_front((self.revision + 1, clip_chars(&line, 240)));
        while self.entries.len() > ACTIVITY_CAPACITY {
            self.entries.pop_back();
        }
        self.revision += 1;
        if let Some(wake) = &self.wake {
            wake.notify();
        }
    }

    pub fn snapshot(&self) -> (u64, Vec<String>) {
        (self.revision, self.entries.iter().map(|(_, line)| line.clone()).collect())
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn set_wake(&mut self, wake: Arc<WakeSignal>) {
        self.wake = Some(wake);
    }
}

/// 插件 -> UI 的提示（命令结果、ui.notify）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub plugin_id: String,
    pub plugin_name: String,
    pub text: String,
    pub level: String,
    /// 目标窗口；`None` = 未解析（UI 回退 active）。
    pub window_id: Option<u64>,
}

/// 插件推送/宿主请求得到的声明式 UI 文档。
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceUpdate {
    pub plugin_id: String,
    pub plugin_name: String,
    /// `"settings"` 或 `"panel:<id>"`。
    pub surface: String,
    pub panel: Option<String>,
    pub doc: SurfaceDoc,
    /// 目标窗口；surface 的两级 key 是 `(window_id, surface)`。
    pub window_id: Option<u64>,
    /// true = 插件主动 `ui.render` 推送（可在目标窗口没有打开 surface 时显示）；
    /// false = 对宿主 `surface.render` 请求的回复。
    pub pushed: bool,
}

/// terminal/clipboard ask：等待用户 Allow/Deny。
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub plugin_id: String,
    pub plugin_name: String,
    pub request_id: u64,
    pub method: String,
    pub summary: String,
    pub details: String,
    pub params: Value,
    pub deadline: Instant,
    /// 发起请求的窗口；Allow/Deny 必须路由回该窗口执行。
    pub window_id: Option<u64>,
}

impl ApprovalRequest {
    /// 展示用：剩余时间（过期返回 0）。
    pub fn seconds_left(&self) -> u64 {
        self.deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
    }
}

/// 宿主对插件提供的非敏感上下文。
#[derive(Clone, Debug, Default)]
pub struct HostSnapshot {
    pub host_version: String,
    pub config_metadata: Value,
    /// UI 线程维护的活动窗口；supervisor 线程解析“缺省 window_id=active”用。
    pub active_window_id: Option<u64>,
}

pub type WakeSlot = Arc<Mutex<Option<Arc<WakeSignal>>>>;

pub(crate) fn notify_wake(slot: &WakeSlot) {
    if let Ok(guard) = slot.lock() {
        if let Some(wake) = guard.as_ref() {
            wake.notify();
        }
    }
}

pub(crate) fn record_activity(
    shared: &SupervisorShared,
    plugin: &str,
    action: &str,
    outcome: &str,
    summary: &str,
) {
    let summary = sanitize_text(summary, MAX_ACTIVITY_SUMMARY_CHARS);
    let line = format!(
        "{} · {} · {} · {} · {}",
        now_hms(),
        sanitize_text(plugin, 80),
        sanitize_text(action, 80),
        sanitize_text(outcome, 40),
        summary
    );
    if let Ok(mut log) = shared.activity.lock() {
        log.push(line);
    }
    notify_wake(&shared.wake);
}

fn push_notice(shared: &SupervisorShared, notice: Notice) {
    if let Ok(mut notices) = shared.notices.lock() {
        if notices.len() >= 32 {
            notices.pop_front();
        }
        notices.push_back(notice);
    }
    notify_wake(&shared.wake);
}

/// 展示文本清理：去掉控制字符，折叠换行/空白。
pub fn sanitize_text(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in text.chars() {
        let ch = if ch.is_control() || ch == '\n' || ch == '\r' || ch == '\t' {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
    }
    clip_chars(out.trim(), max)
}

fn now_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    #[cfg(unix)]
    {
        let time = secs as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r 写入有效的 tm；参数均来自本函数栈/系统时间。
        let result = unsafe { libc::localtime_r(&time, &mut tm) };
        if !result.is_null() {
            return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
        }
    }
    let secs = secs % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

// ---------------------------------------------------------------- 发现

/// 一个磁盘上的插件目录/旧清单条目。v3 之外一律 `supported = false`。
#[derive(Clone, Debug)]
pub struct PluginEntry {
    /// UI 动作使用的稳定 key（v3 = manifest.id；其余 = `legacy/dir:<name>`）。
    pub key: String,
    pub name: String,
    pub version: String,
    pub api_label: String,
    pub dir: PathBuf,
    pub manifest: Option<Manifest>,
    pub entry_path: Option<PathBuf>,
    pub theme_path: Option<PathBuf>,
    pub theme_error: Option<String>,
    pub legacy: bool,
    pub supported: bool,
    pub reason: String,
    /// 由本地安装器写入的元数据（手动放置的插件为 None）。
    pub install_meta: Option<install::InstallMeta>,
    /// 当前 `plugin.toml` + `plugin.toml.sig` 对当前公钥的实时校验结果。
    pub signature_status: signature::SignatureStatus,
}

impl PluginEntry {
    fn v3_key(&self) -> Option<&str> {
        self.manifest.as_ref().map(|manifest| manifest.id.as_str())
    }

    fn runtime_config(&self) -> Result<PluginRuntimeConfig, String> {
        let manifest = self
            .manifest
            .as_ref()
            .ok_or_else(|| format!("{} 不是可执行的 v3 插件", self.name))?;
        let entry = self
            .entry_path
            .clone()
            .ok_or_else(|| format!("{} 缺少可执行 entry", self.name))?;
        Ok(PluginRuntimeConfig {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            dir: self.dir.clone(),
            entry,
            contributes: manifest.contributes.clone(),
            permissions: manifest.permissions.clone(),
            command_ids: manifest.commands.iter().map(|command| command.id.clone()).collect(),
            command_decls: manifest.commands.clone(),
        })
    }

    fn permission_fingerprint(&self) -> Option<Vec<String>> {
        self.manifest
            .as_ref()
            .map(Manifest::permission_fingerprint)
    }
}

/// 发现插件目录；不启动任何进程。旧 v1/v2 清单会进入列表但 `supported=false`。
pub fn discover_plugins(dir: &Path) -> Vec<PluginEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|e| e.path())).collect();
    paths.sort();
    let mut plugins = Vec::new();
    for path in paths {
        // `.staging` / `.backup` 与用户自建隐藏目录都不是可加载插件。
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() && path.join("plugin.toml").is_file() {
            plugins.push(parse_package_dir(&path));
        } else if path.is_file()
            && path.extension().is_some_and(|ext| ext == "toml")
        {
            plugins.push(parse_flat_manifest(&path));
        }
    }
    // 重复 v3 id：保留第一个，其余标记 invalid，避免命令/状态互相覆盖。
    let mut seen: Vec<String> = Vec::new();
    for plugin in &mut plugins {
        if let Some(id) = plugin.v3_key().map(str::to_string) {
            if seen.iter().any(|seen| seen == &id) {
                let reason = format!(
                    "重复插件 id {id:?}（另一个目录已使用该 id）；v3 要求 id 全局唯一"
                );
                plugin.supported = false;
                plugin.reason = reason.clone();
                plugin.manifest = None;
                plugin.entry_path = None;
                plugin.key = format!("invalid:{}", dir_name(&plugin.dir));
            } else {
                seen.push(id);
            }
        }
    }
    plugins.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.key.cmp(&b.key))
    });
    plugins
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin")
        .to_string()
}

fn parse_flat_manifest(path: &Path) -> PluginEntry {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin")
        .to_string();
    let mut entry = PluginEntry {
        key: format!("legacy-flat:{name}"),
        name: name.clone(),
        version: "?".to_string(),
        api_label: "?".to_string(),
        dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        manifest: None,
        entry_path: None,
        theme_path: None,
        theme_error: None,
        legacy: true,
        supported: false,
        reason: "Unsupported legacy flat manifest; Plugin API v3 requires <plugins>/<dir>/plugin.toml with api = 3 and host_abi = 3 (see docs/PLUGIN_V3.md)".to_string(),
        install_meta: None,
        signature_status: signature::SignatureStatus::Unsigned,
    };
    match read_manifest_value(path) {
        Ok(value) => {
            let (display_name, version) = manifest::legacy_display(&value, &name);
            entry.name = display_name;
            entry.version = version;
            entry.api_label = manifest::probe_api(&value)
                .map(|api| api.to_string())
                .unwrap_or_else(|| "1 (legacy)".to_string());
        },
        Err(reason) => {
            entry.api_label = "?".to_string();
            entry.reason = format!("{}; {}", reason, reason_migration());
        },
    }
    entry
}

fn parse_package_dir(dir: &Path) -> PluginEntry {
    let manifest_path = dir.join("plugin.toml");
    let dir_label = dir_name(dir);
    let mut entry = PluginEntry {
        key: format!("legacy:{dir_label}"),
        name: dir_label.clone(),
        version: "?".to_string(),
        api_label: "?".to_string(),
        dir: dir.to_path_buf(),
        manifest: None,
        entry_path: None,
        theme_path: None,
        theme_error: None,
        legacy: false,
        supported: false,
        reason: String::new(),
        install_meta: install::installed_meta(dir),
        signature_status: signature::inspect_plugin_dir(dir),
    };
    let value = match read_manifest_value(&manifest_path) {
        Ok(value) => value,
        Err(reason) => {
            entry.reason = reason;
            return entry;
        },
    };
    let api = manifest::probe_api(&value);
    let (display_name, version) = manifest::legacy_display(&value, &dir_label);
    entry.name = display_name;
    entry.version = version;
    entry.api_label = api
        .map(|api| api.to_string())
        .unwrap_or_else(|| "1 (legacy)".to_string());
    if api != Some(manifest::PLUGIN_API_VERSION as u64) {
        entry.legacy = true;
        entry.reason = format!(
            "Unsupported Plugin API v{}; xtervo v3 only loads api = 3 / host_abi = 3. {}",
            entry.api_label,
            reason_migration()
        );
        return entry;
    }

    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(e) => {
            entry.reason = format!("读取 plugin.toml 失败: {e}");
            return entry;
        },
    };
    let manifest: Manifest = match toml::from_str(&text) {
        Ok(manifest) => manifest,
        Err(e) => {
            entry.reason = format!(
                "v3 plugin.toml 解析失败: {e}. {}",
                reason_migration()
            );
            return entry;
        },
    };
    if let Err(reason) = manifest.validate() {
        entry.reason = format!("v3 清单无效: {reason}");
        return entry;
    }
    let entry_path = match if manifest.is_wasm() {
        manifest::resolve_wasm_entry(dir, &manifest.entry)
    } else {
        manifest::resolve_entry(dir, &manifest.entry)
    } {
        Ok(path) => path,
        Err(reason) => {
            entry.reason = format!("entry 无效: {reason}");
            return entry;
        },
    };
    let mut theme_path = None;
    let mut theme_error = None;
    if let Some(theme) = &manifest.contributes.theme {
        match manifest::resolve_plugin_path(dir, theme, true) {
            Ok(path) => theme_path = Some(path),
            Err(error) => {
                let message = format!("theme 路径无效: {error}");
                eprintln!(
                    "[plugin-v3] {}: {message}（插件其余部分仍可用）",
                    manifest.id
                );
                theme_error = Some(message);
            },
        }
    }
    entry.key = manifest.id.clone();
    entry.name = manifest.name.clone();
    entry.version = manifest.version.clone();
    entry.api_label = manifest.api.to_string();
    entry.manifest = Some(manifest);
    entry.entry_path = Some(entry_path);
    entry.theme_path = theme_path;
    entry.theme_error = theme_error;
    entry.supported = true;
    entry.reason.clear();
    entry.install_meta = install::installed_meta(dir);
    entry.signature_status = signature::inspect_plugin_dir(dir);
    entry
}

fn reason_migration() -> &'static str {
    "v1/v2 plugins are unsupported and are never executed; migrate to api = 3 (docs/PLUGIN_V3.md)."
}

fn read_manifest_value(path: &Path) -> Result<toml::Value, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("读取清单元数据失败 {}: {e}", path.display()))?;
    if metadata.len() > MAX_PLUGIN_MANIFEST_READ {
        return Err(format!(
            "清单文件过大（{} 字节，上限 {}）",
            metadata.len(),
            MAX_PLUGIN_MANIFEST_READ
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读取清单失败 {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("清单不是合法 TOML: {e}"))
}

// ---------------------------------------------------------------- UI 行

/// 设置页 Plugins 的一行（字段与 Slint `PluginRow` 对应）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRow {
    pub key: String,
    pub name: String,
    pub version: String,
    pub api: String,
    /// `process` 或 `wasm`；设置页显示，便于区分热路径过滤器。
    pub runtime: String,
    pub status: String,
    pub error: String,
    pub permissions: String,
    pub restart_count: i32,
    pub enabled: bool,
    /// enabled 但权限指纹未确认（权限变化/首次启用后还没点确认）。
    pub needs_approval: bool,
    pub supported: bool,
    pub has_panel: bool,
    pub has_settings: bool,
    pub assistant: bool,
    /// 面板/settings 是否可用（打开按钮据此显示）。
    pub surface_ready: bool,
    /// 安装来源（安装器写入；手动放置显示 manual）。
    pub source: String,
    /// 实时签名状态（verified / unsigned / invalid / unverifiable）。
    pub signature: String,
    /// 是否有与当前版本不同的可回滚备份。
    pub has_backup: bool,
    /// 默认 Rollback 会回到的版本（无备份时为空）。
    pub backup_version: String,
}

/// 状态栏一条（一个插件最多一条）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusItem {
    pub plugin_id: String,
    pub plugin_name: String,
    pub text: String,
    pub tooltip: String,
    /// 完整命令 ID（`plugin.<id>.<cmd>`），已校验在 manifest 中声明。
    pub command: Option<String>,
}

/// enable 请求结果：第一次启用必须先给用户看权限并确认。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnableDecision {
    Approved,
    NeedsApproval { permissions: Vec<String> },
}

/// 已启用且权限已确认的插件命令（Palette / Hotkeys 页 / 按键分发共用）。
///
/// 每次调用都从 `entries` + 持久化状态现算：Disable/Reload 后下一次查询即失效，
/// 不需要单独维护一份“已注册命令”缓存，也就不存在清理遗漏。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCommandRef {
    pub full_id: String,
    pub plugin_id: String,
    pub plugin_name: String,
    pub title: String,
    pub description: String,
    /// 清单默认快捷键（canonical 小写格式）；空 = 未绑定。
    pub keybinding: Option<String>,
    /// `when` 条件原文；空 = 总是可用。
    pub when: String,
}

/// 清单 `when` 对当前上下文是否成立；解析过的非法表达式按 false（不可用）处理，
/// 避免坏清单让命令在错误上下文里执行。
pub fn command_when_allows(decl: &CommandDecl, ctx: &CommandContext) -> bool {
    let when = decl.when.trim();
    if when.is_empty() {
        return true;
    }
    WhenExpr::parse(when).map(|expr| expr.eval(ctx)).unwrap_or(false)
}

// ---------------------------------------------------------------- PluginStore

#[cfg(test)]
static TEST_SIDECAR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct PluginStore {
    dir: PathBuf,
    state_path: PathBuf,
    options: SupervisorOptions,
    shared: SupervisorShared,
    entries: Vec<PluginEntry>,
    index: HashMap<String, usize>,
    state: PersistedState,
    state_error: Option<String>,
    supervisors: HashMap<String, SupervisorHandle>,
    /// 已启用 WASM 输出过滤器的实例；按插件 id 索引。
    wasm_filters: HashMap<String, Arc<WasmFilterRuntime>>,
    /// 交给 core pane 的共享热路径句柄；Enable/Disable/Reload 原地替换。
    wasm_host: Arc<WasmOutputHost>,
    last_errors: HashMap<String, String>,
    interns: HashMap<String, &'static str>,
    started: bool,
    /// 测试专用：串行化会 spawn sidecar 的测试，避免并行跑几十个插件进程
    /// 导致资源竞争/偶发重启。guard 随 PluginStore drop 释放。
    #[cfg(test)]
    test_guard: Option<std::sync::MutexGuard<'static, ()>>,
}

impl PluginStore {
    pub fn dir() -> PathBuf {
        if let Ok(path) = std::env::var("XTERVO_PLUGINS") {
            return PathBuf::from(path);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                    .join(".config")
            });
        base.join("xtervo").join("plugins")
    }

    pub fn state_path() -> PathBuf {
        crate::config::Config::path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("plugins-v3-state.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::dir(), &Self::state_path())
    }

    /// 安全模式：照常读 manifest/状态（设置页仍能看到列表），但**不保留任何条目**，
    /// 因此不会启动插件进程。用于连续多次异常退出后的降级启动（ACT-036）。
    pub fn suppressed_for_safe_mode() -> Self {
        let mut store = Self::load();
        let count = store.entries.len();
        store.entries.clear();
        store.index.clear();
        if count > 0 {
            eprintln!("[safe-mode] 已屏蔽 {count} 个插件（连续异常退出降级）");
        }
        store
    }

    pub fn load_from(dir: &Path, state_path: &Path) -> Self {
        Self::load_with_options(dir, state_path, SupervisorOptions::default())
    }

    /// 测试钩子：允许缩短超时；生产只使用默认值。
    pub fn load_with_options(
        dir: &Path,
        state_path: &Path,
        options: SupervisorOptions,
    ) -> Self {
        let entries = discover_plugins(dir);
        let (state, state_error) = load_state(state_path);
        if let Some(error) = &state_error {
            eprintln!("[plugin-v3] 状态文件: {error}");
        }
        let mut index = HashMap::new();
        for (position, entry) in entries.iter().enumerate() {
            index.insert(entry.key.clone(), position);
        }
        let shared = SupervisorShared {
            activity: Arc::new(Mutex::new(ActivityLog::default())),
            notices: Arc::new(Mutex::new(VecDeque::new())),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            windows: Arc::new(Mutex::new(HashSet::new())),
            surfaces: Arc::new(Mutex::new(VecDeque::new())),
            approvals: Arc::new(Mutex::new(VecDeque::new())),
            host: Arc::new(Mutex::new(HostSnapshot {
                host_version: env!("CARGO_PKG_VERSION").to_string(),
                config_metadata: json!({}),
                active_window_id: None,
            })),
            status_revision: Arc::new(AtomicU64::new(0)),
            runtime_revision: Arc::new(AtomicU64::new(0)),
            wake: Arc::new(Mutex::new(None)),
        };
        Self {
            dir: dir.to_path_buf(),
            state_path: state_path.to_path_buf(),
            options,
            shared,
            entries,
            index,
            state,
            state_error,
            supervisors: HashMap::new(),
            wasm_filters: HashMap::new(),
            wasm_host: Arc::new(WasmOutputHost::default()),
            last_errors: HashMap::new(),
            interns: HashMap::new(),
            started: false,
            #[cfg(test)]
            test_guard: None,
        }
    }

    pub fn plugin_dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &[PluginEntry] {
        &self.entries
    }

    pub fn state_error(&self) -> Option<&str> {
        self.state_error.as_deref()
    }

    /// 发现过至少一个 WASM 插件时返回共享热路径句柄。即使当前全部禁用也返回
    /// 句柄：之后 Enable 会原地替换内部实例，已创建的 pane 立即生效，不需要
    /// 重建 PTY 或重启 xtervo。
    pub fn output_filter_handle(&self) -> Option<OutputFilterHandle> {
        self.has_wasm_plugins()
            .then(|| OutputFilterHandle::new(self.wasm_host.clone()))
    }

    pub fn has_wasm_plugins(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.manifest.as_ref().is_some_and(Manifest::is_wasm))
    }

    /// 已激活的 WASM 过滤器 id（按字典序，即热路径顺序）。
    pub fn active_wasm_filters(&self) -> Vec<String> {
        self.wasm_host.active_ids()
    }

    /// 热路径统计快照（测试/soak 证据）。
    pub fn wasm_filter_stats(&self, key: &str) -> Option<WasmFilterStatsSnapshot> {
        self.wasm_identity(key)
            .and_then(|id| self.wasm_host.stats(id.as_str()))
    }

    fn wasm_identity(&self, key: &str) -> Option<String> {
        self.entry_index(key)
            .and_then(|position| self.entries.get(position))
            .and_then(PluginEntry::v3_key)
            .map(str::to_string)
    }

    fn stop_wasm(&mut self, id: &str) {
        self.wasm_host.remove(id);
        self.wasm_filters.remove(id);
    }

    fn stop_runtime(&mut self, id: &str) {
        self.stop_wasm(id);
        if let Some(handle) = self.supervisors.remove(id) {
            handle.shutdown();
        }
    }

    /// 注入 UI 唤醒（容量 1，后台线程只 notify，不碰 Slint）。
    pub fn set_activity_wake(&self, wake: WakeSignal) {
        let wake = Arc::new(wake);
        if let Ok(mut log) = self.shared.activity.lock() {
            log.set_wake(wake.clone());
        }
        if let Ok(mut slot) = self.shared.wake.lock() {
            *slot = Some(wake);
        }
    }

    /// 只给插件非敏感配置元数据（theme/language/font_size 等由 main 构造）。
    pub fn set_config_metadata(&self, metadata: Value) {
        if let Ok(mut host) = self.shared.host.lock() {
            host.config_metadata = metadata;
        }
    }

    /// UI 线程同步活动窗口；supervisor 用它解析缺省 `window_id`。
    /// active 窗口一定真实存在，因此同时登记到已知窗口集合。
    pub fn set_active_window(&self, window_id: Option<u64>) {
        if let Some(window_id) = window_id {
            self.register_window(window_id);
        }
        if let Ok(mut host) = self.shared.host.lock() {
            host.active_window_id = window_id;
        }
    }

    /// 登记一个真实存在的窗口；插件显式 `window_id` 必须命中该集合。
    pub fn register_window(&self, window_id: u64) {
        if let Ok(mut windows) = self.shared.windows.lock() {
            windows.insert(window_id);
        }
    }

    /// UI 线程按 `WindowRegistry` 全量同步窗口集合（每次 UI 刷新调用）。
    /// 只保留当前真实窗口，避免已关闭 id 继续被插件命中。
    pub fn set_known_windows(&self, window_ids: &[u64]) {
        if let Ok(mut windows) = self.shared.windows.lock() {
            windows.clear();
            windows.extend(window_ids.iter().copied());
        }
    }

    /// 测试/诊断：当前登记的窗口 id（升序）。
    pub fn known_windows(&self) -> Vec<u64> {
        let mut windows = self
            .shared
            .windows
            .lock()
            .map(|windows| windows.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        windows.sort_unstable();
        windows
    }

    pub fn active_window(&self) -> Option<u64> {
        self.shared
            .host
            .lock()
            .ok()
            .and_then(|host| host.active_window_id)
    }

    /// 运行状态（测试/诊断）；key 为 UI 行 key（v3 = manifest.id）。
    pub fn runtime_info(&self, key: &str) -> Option<RuntimeInfo> {
        let id = self
            .entry_index(key)
            .and_then(|position| self.entries.get(position))
            .and_then(PluginEntry::v3_key)?;
        if let Some(runtime) = self.wasm_host.get(id) {
            return Some(RuntimeInfo {
                phase: Some(if runtime.is_faulted() {
                    Phase::Failed
                } else {
                    Phase::Running
                }),
                error: runtime.fault_error().unwrap_or_default(),
                restart_count: 0,
                pid: None,
            });
        }
        self.supervisors.get(id).map(SupervisorHandle::runtime)
    }

    pub fn phase(&self, key: &str) -> Option<Phase> {
        self.runtime_info(key).map(|info| info.phase())
    }

    pub fn is_approved(&self, key: &str) -> bool {
        self.entry_index(key)
            .and_then(|position| self.entries.get(position))
            .and_then(|entry| {
                entry
                    .v3_key()
                    .zip(entry.permission_fingerprint())
            })
            .is_some_and(|(id, permissions)| self.state.approved_for(id, &permissions))
    }

    pub fn total_count(&self) -> usize {
        self.entries.len()
    }

    pub fn enabled_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| {
                entry.manifest.as_ref().is_some_and(|manifest| {
                    self.state.is_enabled(&manifest.id)
                })
            })
            .count()
    }

    pub fn enabled_command_count(&self) -> usize {
        self.entries
            .iter()
            .filter_map(|entry| entry.manifest.as_ref())
            .filter(|manifest| self.state.is_enabled(&manifest.id))
            .map(|manifest| manifest.commands.len())
            .sum()
    }

    pub fn unsupported_count(&self) -> usize {
        self.entries.iter().filter(|entry| !entry.supported).count()
    }

    pub fn activity_snapshot(&self) -> (u64, Vec<String>) {
        self.shared
            .activity
            .lock()
            .map(|log| log.snapshot())
            .unwrap_or((0, Vec::new()))
    }

    pub fn runtime_revision(&self) -> u64 {
        self.shared.runtime_revision.load(Ordering::SeqCst)
    }

    pub fn status_revision(&self) -> u64 {
        self.shared.status_revision.load(Ordering::SeqCst)
    }

    fn bump_runtime(&self) {
        self.shared.runtime_revision.fetch_add(1, Ordering::SeqCst);
        notify_wake(&self.shared.wake);
    }

    fn entry_index(&self, key: &str) -> Option<usize> {
        self.index.get(key).copied()
    }

    fn v3_manifest(&self, key: &str) -> Result<(usize, Manifest), String> {
        let index = self
            .entry_index(key)
            .ok_or_else(|| format!("插件不存在: {key}"))?;
        let entry = &self.entries[index];
        let manifest = entry
            .manifest
            .as_ref()
            .ok_or_else(|| format!("插件 {key} 不是受支持的 v3 插件: {}", entry.reason))?
            .clone();
        Ok((index, manifest))
    }

    /// 设置页行快照（只读；supervisor 运行时状态实时读取）。
    pub fn rows(&self) -> Vec<PluginRow> {
        self.entries
            .iter()
            .map(|entry| {
                let permissions = entry
                    .manifest
                    .as_ref()
                    .map(|manifest| manifest.permissions.summary())
                    .unwrap_or_else(|| "—".to_string());
                let (supported, has_panel, has_settings, assistant, runtime_label) = entry
                    .manifest
                    .as_ref()
                    .map(|manifest| {
                        (
                            entry.supported,
                            !manifest.contributes.panels.is_empty(),
                            manifest.contributes.settings,
                            manifest.contributes.assistant_enabled(),
                            manifest.runtime.trim().to_string(),
                        )
                    })
                    .unwrap_or((false, false, false, false, "—".to_string()));
                let id = entry.v3_key();
                let persisted = id
                    .map(|id| self.state.entry(id))
                    .unwrap_or_default();
                let approved = id
                    .zip(entry.permission_fingerprint())
                    .is_some_and(|(id, permissions)| {
                        self.state.approved_for(id, &permissions)
                    });
                let runtime = self.runtime_info(&entry.key);
                let enabled = supported && persisted.enabled;
                let status = if !supported {
                    if entry.legacy {
                        "unsupported".to_string()
                    } else {
                        "invalid".to_string()
                    }
                } else if !enabled {
                    "disabled".to_string()
                } else if !approved {
                    "permission confirmation required".to_string()
                } else if let Some(runtime) = &runtime {
                    runtime.phase().as_str().to_string()
                } else if self.last_errors.contains_key(entry.key.as_str()) {
                    "failed".to_string()
                } else {
                    "stopped".to_string()
                };
                let error = if !supported {
                    clip_chars(&entry.reason, MAX_ERROR_TEXT_CHARS)
                } else if let Some(runtime) = &runtime {
                    if !runtime.error.trim().is_empty() {
                        clip_chars(&runtime.error, MAX_ERROR_TEXT_CHARS)
                    } else if let Some(error) = id.and_then(|id| self.last_errors.get(id)) {
                        clip_chars(error, MAX_ERROR_TEXT_CHARS)
                    } else if let Some(error) = &entry.theme_error {
                        clip_chars(error, MAX_ERROR_TEXT_CHARS)
                    } else if enabled && !approved {
                        "Permissions changed or not confirmed yet. Click Enable to review and confirm."
                            .to_string()
                    } else {
                        String::new()
                    }
                } else if let Some(error) = id.and_then(|id| self.last_errors.get(id)) {
                    clip_chars(error, MAX_ERROR_TEXT_CHARS)
                } else if let Some(error) = &entry.theme_error {
                    clip_chars(error, MAX_ERROR_TEXT_CHARS)
                } else if enabled && !approved {
                    "Permissions changed or not confirmed yet. Click Enable to review and confirm."
                        .to_string()
                } else {
                    String::new()
                };
                let backups = id
                    .map(|id| install::list_backups(&self.dir, id))
                    .unwrap_or_default();
                let rollback_target = backups
                    .iter()
                    .find(|backup| backup.version != entry.version)
                    .or_else(|| backups.first());
                let source = entry
                    .install_meta
                    .as_ref()
                    .map(|meta| meta.source.clone())
                    .unwrap_or_else(|| "manual (not installed by xtervo)".to_string());
                let signature_label = match (&entry.signature_status, &entry.install_meta) {
                    (signature::SignatureStatus::Unsigned, Some(meta)) => meta.signature_label(),
                    (signature::SignatureStatus::Unsigned, None) => {
                        "unsigned · installed manually".to_string()
                    },
                    (status, _) => status.label(),
                };
                PluginRow {
                    key: entry.key.clone(),
                    name: entry.name.clone(),
                    version: entry.version.clone(),
                    api: entry.api_label.clone(),
                    runtime: runtime_label,
                    status,
                    error,
                    permissions,
                    restart_count: runtime.map(|runtime| runtime.restart_count as i32).unwrap_or(0),
                    enabled,
                    needs_approval: supported && enabled && !approved,
                    supported,
                    has_panel,
                    has_settings,
                    assistant,
                    surface_ready: supported && approved && persisted.enabled,
                    source: sanitize_text(&source, 320),
                    signature: sanitize_text(&signature_label, 240),
                    has_backup: supported
                        && backups
                            .iter()
                            .any(|backup| backup.version != entry.version),
                    backup_version: rollback_target
                        .map(|backup| backup.version.clone())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    pub fn take_notices(&self) -> Vec<Notice> {
        self.shared
            .notices
            .lock()
            .map(|mut notices| notices.drain(..).collect())
            .unwrap_or_default()
    }

    /// 只取目标窗口的提示；其它窗口的留在队列里，避免先轮询的窗口吞掉。
    pub fn take_notices_for(&self, window_id: u64, active: Option<u64>) -> Vec<Notice> {
        take_matching(&self.shared.notices, |notice| {
            window_matches_target(notice.window_id, window_id, active)
        })
    }

    pub fn take_surfaces(&self) -> Vec<SurfaceUpdate> {
        self.shared
            .surfaces
            .lock()
            .map(|mut surfaces| surfaces.drain(..).collect())
            .unwrap_or_default()
    }

    /// 只取 `(window_id, surface)` 目标窗口的 surface 更新。
    pub fn take_surfaces_for(&self, window_id: u64, active: Option<u64>) -> Vec<SurfaceUpdate> {
        take_matching(&self.shared.surfaces, |update| {
            window_matches_target(update.window_id, window_id, active)
        })
    }

    pub fn pending_approvals(&self) -> Vec<ApprovalRequest> {
        self.shared
            .approvals
            .lock()
            .map(|approvals| approvals.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 该窗口应展示的 ask；不消费队列（Allow/Deny 后由 resolve 移除）。
    pub fn pending_approvals_for(&self, window_id: u64, active: Option<u64>) -> Vec<ApprovalRequest> {
        self.shared
            .approvals
            .lock()
            .map(|approvals| {
                approvals
                    .iter()
                    .filter(|approval| {
                        window_matches_target(approval.window_id, window_id, active)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 关闭窗口时清理该窗口的 surface/status/notice/ask，避免残留状态串到新窗口。
    ///
    /// 返回被取消的 ask 数；对应的插件请求收到 `UNAVAILABLE (window closed)`，
    /// 不会永远挂着等超时。
    pub fn forget_window(&self, window_id: u64) -> usize {
        if let Ok(mut windows) = self.shared.windows.lock() {
            windows.remove(&window_id);
        }
        let dropped_approvals: Vec<ApprovalRequest> = {
            match self.shared.approvals.lock() {
                Ok(mut approvals) => {
                    let mut kept = VecDeque::with_capacity(approvals.len());
                    let mut dropped = Vec::new();
                    for approval in approvals.drain(..) {
                        if approval.window_id == Some(window_id) {
                            dropped.push(approval);
                        } else {
                            kept.push_back(approval);
                        }
                    }
                    *approvals = kept;
                    dropped
                },
                Err(_) => Vec::new(),
            }
        };
        let status_changed = match self.shared.statuses.lock() {
            Ok(mut statuses) => {
                let before = statuses.len();
                statuses.retain(|(_, window), _| *window != Some(window_id));
                statuses.len() != before
            },
            Err(_) => false,
        };
        if let Ok(mut surfaces) = self.shared.surfaces.lock() {
            surfaces.retain(|update| update.window_id != Some(window_id));
        }
        if let Ok(mut notices) = self.shared.notices.lock() {
            notices.retain(|notice| notice.window_id != Some(window_id));
        }
        if status_changed {
            self.shared.status_revision.fetch_add(1, Ordering::SeqCst);
        }
        notify_wake(&self.shared.wake);
        for approval in &dropped_approvals {
            let _ = self.resolve_host_request(
                &approval.plugin_id,
                approval.request_id,
                HostOutcome::Unavailable("window closed".to_string()),
            );
        }
        eprintln!(
            "[plugin-v3] forget window={window_id} surfaces/status/notices cleared，取消 ask={}",
            dropped_approvals.len()
        );
        dropped_approvals.len()
    }

    /// UI 对 ask 的最终答复（terminal/clipboard 的实际读写由 UI 线程完成）。
    pub fn resolve_host_request(
        &self,
        plugin_id: &str,
        request_id: u64,
        outcome: HostOutcome,
    ) -> bool {
        if let Ok(mut approvals) = self.shared.approvals.lock() {
            approvals.retain(|approval| {
                !(approval.plugin_id == plugin_id && approval.request_id == request_id)
            });
        }
        self.supervisors
            .get(plugin_id)
            .is_some_and(|handle| {
                handle
                    .tx
                    .send(SupervisorMsg::HostReply { id: request_id, outcome })
                    .is_ok()
            })
    }

    /// 全部状态（诊断/测试用）；产品 UI 用 [`Self::status_snapshot_for_window`]。
    pub fn status_snapshot(&self) -> (u64, Vec<StatusItem>) {
        self.status_snapshot_inner(None)
    }

    /// 该窗口的状态栏：全局状态（status.get 轮询）+ 该窗口的 ui.status 覆盖。
    pub fn status_snapshot_for_window(&self, window_id: u64) -> (u64, Vec<StatusItem>) {
        self.status_snapshot_inner(Some(window_id))
    }

    fn status_snapshot_inner(&self, window_id: Option<u64>) -> (u64, Vec<StatusItem>) {
        let revision = self.status_revision();
        // 不 clone 整张 status 表：只复制目标窗口相关的条目（每插件最多 3 条），
        // 否则插件伪造窗口 id 时 UI 每次刷新都要复制全表。
        let mut effective: HashMap<String, (Option<u64>, StatusPayload)> = HashMap::new();
        if let Ok(statuses) = self.shared.statuses.lock() {
            for ((plugin_id, target), status) in statuses.iter() {
                let relevant = match window_id {
                    Some(window) => target.is_none() || *target == Some(window),
                    None => true,
                };
                if !relevant {
                    continue;
                }
                match effective.get(plugin_id.as_str()) {
                    Some((existing, _)) if existing.is_some() && target.is_none() => {},
                    _ => {
                        effective.insert(plugin_id.clone(), (*target, status.clone()));
                    },
                }
            }
        }
        let mut items: Vec<StatusItem> = effective
            .into_iter()
            .filter_map(|(plugin_id, (_, status))| {
                let entry = self
                    .entries
                    .iter()
                    .find(|entry| entry.v3_key() == Some(plugin_id.as_str()))?;
                let manifest = entry.manifest.as_ref()?;
                if !self.state.is_enabled(&plugin_id) {
                    return None;
                }
                let command = status.command.as_ref().and_then(|command| {
                    manifest
                        .commands
                        .iter()
                        .any(|decl| &decl.id == command)
                        .then(|| manifest.full_command_id(command))
                });
                Some(StatusItem {
                    plugin_id: plugin_id.clone(),
                    plugin_name: manifest.name.clone(),
                    text: status.text,
                    tooltip: status.tooltip,
                    command,
                })
            })
            .collect();
        items.sort_by(|a, b| a.plugin_name.cmp(&b.plugin_name));
        (revision, items)
    }

    // ------------------------------------------------------------ 生命周期

    /// 启动所有已启用且权限已确认的插件；不阻塞首屏（每个插件独立线程）。
    pub fn start_process_plugins(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        let mut started = 0;
        let mut awaiting = 0;
        for position in 0..self.entries.len() {
            let Some(manifest) = self.entries[position].manifest.clone() else {
                continue;
            };
            if !self.state.is_enabled(&manifest.id) {
                continue;
            }
            if !self
                .state
                .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                awaiting += 1;
                continue;
            }
            if let Err(error) = self.start_plugin_at(position) {
                eprintln!("[plugin-v3] {}: 启动失败: {error}", manifest.id);
                self.last_errors.insert(manifest.id.clone(), error);
            } else {
                started += 1;
            }
        }
        eprintln!(
            "[plugin-v3] 已发现 {} 个插件（{} unsupported/invalid），启动 {started} 个已确认插件，{awaiting} 个等待权限确认",
            self.entries.len(),
            self.unsupported_count()
        );
    }

    fn start_plugin_at(&mut self, position: usize) -> Result<(), String> {
        let manifest = self
            .entries
            .get(position)
            .and_then(|entry| entry.manifest.as_ref())
            .ok_or_else(|| "插件索引越界或不是 v3 插件".to_string())?;
        if manifest.is_wasm() {
            self.start_wasm_at(position)
        } else {
            self.start_supervisor_at(position)
        }
    }

    /// 读取、编译并激活一个 WASM 输出过滤器。同步执行（仅 Enable/Reload 用户
    /// 动作），热路径调用不在 UI 线程。失败时停止旧实例并记录错误；输出原样
    /// 放行，xtervo 继续运行。
    fn start_wasm_at(&mut self, position: usize) -> Result<(), String> {
        let entry = self
            .entries
            .get(position)
            .ok_or_else(|| "插件索引越界".to_string())?
            .clone();
        let manifest = entry
            .manifest
            .clone()
            .ok_or_else(|| format!("{} 不是 v3 插件", entry.name))?;
        if !manifest.is_wasm() {
            return Err(format!("{} 不是 runtime = \"wasm\"", manifest.id));
        }
        if !entry.supported {
            return Err(format!("{} 不受支持: {}", entry.name, entry.reason));
        }
        // runtime 可能从 process reload 成 wasm；旧 process 必须先停。
        if let Some(handle) = self.supervisors.remove(&manifest.id) {
            handle.shutdown();
        }
        // 先停旧过滤器：磁盘上的模块已经变化，不能继续跑旧实例。
        self.stop_wasm(&manifest.id);
        let path = entry
            .entry_path
            .clone()
            .ok_or_else(|| format!("{} 缺少 .wasm entry", manifest.id))?;
        // 先 stat 再读，且最多读 MAX+1 字节：发现后文件被替换/放大也不会把
        // 整个超大文件读进 UI 线程。
        let bytes = match crate::plugin_wasm::read_wasm_module(
            &path,
            crate::plugin_wasm::MAX_WASM_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                // helper 的错误文本已带路径/上限信息。
                self.last_errors.insert(manifest.id.clone(), error.clone());
                return Err(error);
            },
        };
        let runtime = match WasmFilterRuntime::load(&manifest.id, &bytes) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                let message = format!("加载 WASM 失败 {}: {error}", path.display());
                self.last_errors.insert(manifest.id.clone(), message.clone());
                return Err(message);
            },
        };
        self.wasm_host.activate(runtime.clone());
        self.wasm_filters.insert(manifest.id.clone(), runtime);
        self.last_errors.remove(&manifest.id);
        self.bump_runtime();
        eprintln!(
            "[plugin-v3] {}: WASM 输出过滤器已激活（{} bytes，input/output ≤ {} KiB）",
            manifest.id,
            bytes.len(),
            crate::plugin_wasm::MAX_INPUT_BYTES / 1024
        );
        Ok(())
    }

    fn start_supervisor_at(&mut self, position: usize) -> Result<(), String> {
        let entry = self
            .entries
            .get(position)
            .ok_or_else(|| "插件索引越界".to_string())?
            .clone();
        let manifest = entry
            .manifest
            .clone()
            .ok_or_else(|| format!("{} 不是 v3 插件", entry.name))?;
        if !entry.supported {
            return Err(format!("{} 不受支持: {}", entry.name, entry.reason));
        }
        if manifest.is_wasm() {
            return Err("WASM 插件不能启动 process supervisor".to_string());
        }
        // runtime 可能从 wasm reload 成 process；旧热路径实例必须先移除。
        self.stop_wasm(&manifest.id);
        if let Some(handle) = self.supervisors.get(&manifest.id) {
            if matches!(
                handle.phase(),
                Phase::Starting | Phase::Running | Phase::Restarting
            ) {
                return Ok(());
            }
        }
        if let Some(old) = self.supervisors.remove(&manifest.id) {
            old.shutdown();
        }
        let config = match entry.runtime_config() {
            Ok(config) => config,
            Err(error) => {
                self.last_errors.insert(manifest.id.clone(), error.clone());
                return Err(error);
            },
        };
        let handle = match spawn_supervisor(
            config,
            self.shared.clone(),
            self.options.clone(),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.last_errors.insert(manifest.id.clone(), error.clone());
                return Err(error);
            },
        };
        self.supervisors.insert(manifest.id.clone(), handle);
        self.last_errors.remove(&manifest.id);
        self.bump_runtime();
        Ok(())
    }

    /// UI 点 Enable：权限未确认时只返回权限列表，不启动进程。
    pub fn request_enable(&mut self, key: &str) -> Result<EnableDecision, String> {
        let (position, manifest) = self.v3_manifest(key)?;
        let permissions = manifest.permission_fingerprint();
        if !self.state.approved_for(&manifest.id, &permissions) {
            return Ok(EnableDecision::NeedsApproval { permissions });
        }
        self.state.set_enabled(&manifest.id, true);
        self.persist_state();
        self.start_plugin_at(position)?;
        self.bump_runtime();
        Ok(EnableDecision::Approved)
    }

    /// 用户在确认弹窗点 Allow：持久化权限指纹，然后启动。
    pub fn confirm_enable(&mut self, key: &str) -> Result<(), String> {
        let (position, manifest) = self.v3_manifest(key)?;
        self.state.approve(&manifest.id, manifest.permission_fingerprint());
        self.state.set_enabled(&manifest.id, true);
        self.persist_state();
        self.start_plugin_at(position)?;
        self.bump_runtime();
        eprintln!(
            "[plugin-v3] {}: 用户已确认权限 [{}]，enabled=true",
            manifest.id,
            manifest.permissions.summary()
        );
        Ok(())
    }

    pub fn disable_plugin(&mut self, key: &str) -> Result<(), String> {
        let (_, manifest) = self.v3_manifest(key)?;
        self.state.set_enabled(&manifest.id, false);
        self.persist_state();
        self.stop_runtime(&manifest.id);
        if let Ok(mut approvals) = self.shared.approvals.lock() {
            approvals.retain(|approval| approval.plugin_id != manifest.id);
        }
        self.bump_runtime();
        eprintln!("[plugin-v3] {}: disabled（xtervo 不重启）", manifest.id);
        Ok(())
    }

    /// 重新读盘、校验权限指纹；权限变化会停止 runtime 并要求重新确认。
    /// process 与 wasm 共用同一条生命周期，但分别走 supervisor / 热路径实例。
    pub fn reload_plugin(&mut self, key: &str) -> Result<(), String> {
        let position = self
            .entry_index(key)
            .ok_or_else(|| format!("插件不存在: {key}"))?;
        let dir = self.entries[position].dir.clone();
        let old_id = self.entries[position].v3_key().map(str::to_string);
        let replacement = parse_package_dir(&dir);
        let new_id = replacement.v3_key().map(str::to_string);
        self.entries[position] = replacement.clone();
        self.rebuild_index();
        if !replacement.supported || new_id.is_none() {
            if let Some(old_id) = old_id {
                self.stop_runtime(&old_id);
            }
            self.bump_runtime();
            let reason = replacement.reason.clone();
            eprintln!("[plugin-v3] {}: reload 失败: {reason}", replacement.name);
            return Err(reason);
        }
        let new_id = new_id.unwrap();
        if let Some(old_id) = old_id {
            if old_id != new_id {
                self.stop_runtime(&old_id);
            }
        }
        let manifest = replacement.manifest.as_ref().expect("supported manifest");
        let permissions = manifest.permission_fingerprint();
        if !self.state.approved_for(&new_id, &permissions) {
            self.state.revoke_approval(&new_id);
            self.stop_runtime(&new_id);
            self.persist_state();
            self.bump_runtime();
            push_notice(&self.shared, Notice {
                plugin_id: new_id.clone(),
                plugin_name: manifest.name.clone(),
                text: "Permissions changed or were not confirmed. Click Enable to review and confirm before the plugin runs.".to_string(),
                level: "warn".to_string(),
                window_id: None,
            });
            eprintln!(
                "[plugin-v3] {new_id}: reload 后权限需要重新确认 [{}]",
                manifest.permissions.summary()
            );
            return Ok(());
        }
        if self.state.is_enabled(&new_id) {
            if manifest.is_wasm() {
                // 编译器会同步重新读盘/实例化；失败时输出原样放行并在设置页显示错误。
                self.start_wasm_at(position)?;
                self.bump_runtime();
                eprintln!("[plugin-v3] {new_id}: WASM reload 完成（xtervo 未重启）");
                return Ok(());
            }
            // process：先移除可能存在的旧 wasm 实例（runtime 类型变化）。
            self.stop_wasm(&new_id);
            if let Some(handle) = self.supervisors.get(&new_id) {
                if matches!(
                    handle.phase(),
                    Phase::Starting | Phase::Running | Phase::Restarting
                ) {
                    match replacement.runtime_config() {
                        Ok(config) => {
                            handle.reload(Box::new(config));
                            self.bump_runtime();
                            eprintln!("[plugin-v3] {new_id}: reload 已提交（不重启 xtervo）");
                            return Ok(());
                        },
                        Err(error) => {
                            self.stop_runtime(&new_id);
                            self.bump_runtime();
                            return Err(error);
                        },
                    }
                }
            }
            self.start_supervisor_at(position)?;
        } else {
            self.stop_runtime(&new_id);
        }
        self.bump_runtime();
        eprintln!("[plugin-v3] {new_id}: reload 完成（xtervo 未重启）");
        Ok(())
    }

    pub fn restart_plugin(&mut self, key: &str) -> Result<(), String> {
        let (position, manifest) = self.v3_manifest(key)?;
        if !self.state.is_enabled(&manifest.id) {
            return Err("插件未启用；先 Enable".to_string());
        }
        if !self
            .state
            .approved_for(&manifest.id, &manifest.permission_fingerprint())
        {
            return Err("权限尚未确认；先 Enable 并确认".to_string());
        }
        if manifest.is_wasm() {
            self.start_wasm_at(position)?;
            self.bump_runtime();
            eprintln!(
                "[plugin-v3] {}: WASM filter 重新加载（不重启 xtervo）",
                manifest.id
            );
            return Ok(());
        }
        if let Some(handle) = self.supervisors.get(&manifest.id) {
            if matches!(
                handle.phase(),
                Phase::Starting | Phase::Running | Phase::Restarting
            ) {
                handle.restart();
                self.bump_runtime();
                eprintln!("[plugin-v3] {}: manual restart（不重启 xtervo）", manifest.id);
                return Ok(());
            }
        }
        self.start_supervisor_at(position)?;
        self.bump_runtime();
        eprintln!("[plugin-v3] {}: restart 新 supervisor（不重启 xtervo）", manifest.id);
        Ok(())
    }

    /// 关闭全部插件进程/WASM 实例；窗口退出/Drop 调用。最多等 1.2s。
    pub fn shutdown_processes(&mut self) {
        // 先摘掉共享热路径，后续 actor 调用立即 passthrough；再释放实例/线程。
        self.wasm_host.clear();
        self.wasm_filters.clear();
        for handle in self.supervisors.values() {
            handle.shutdown();
        }
        let deadline = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < deadline {
            let busy = self.supervisors.values().any(|handle| {
                matches!(
                    handle.phase(),
                    Phase::Starting | Phase::Running | Phase::Restarting
                )
            });
            if !busy {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("[plugin-v3] 关闭超时：仍有插件进程未退出");
    }

    /// 本地安装/更新（不执行插件代码）。成功后刷新条目；启用状态下会重载 host。
    pub fn install_local(
        &mut self,
        source: &Path,
        update: bool,
        require_signature: bool,
        backup_keep: usize,
    ) -> Result<InstallReport, String> {
        let options = InstallOptions {
            require_signature,
            backup_keep,
            update,
            ..InstallOptions::default()
        };
        let report = install_package(&self.dir, source, &options)?;
        self.refresh_after_install(&report.id);
        eprintln!(
            "[plugin-v3] {} {} -> {}（{}，{} 文件，{} 字节）",
            if update { "updated" } else { "installed" },
            report.id,
            report.version,
            report.signature.label(),
            report.files,
            report.bytes
        );
        for warning in &report.warnings {
            eprintln!("[plugin-v3] install warning: {warning}");
        }
        Ok(report)
    }

    /// 回滚到 `.backup/<id>/` 的旧版本；成功后刷新条目。
    pub fn rollback_local(
        &mut self,
        key: &str,
        to: Option<&str>,
        require_signature: bool,
        backup_keep: usize,
    ) -> Result<RollbackReport, String> {
        let id = self
            .entry_index(key)
            .and_then(|position| self.entries.get(position))
            .and_then(PluginEntry::v3_key)
            .map(str::to_string)
            .unwrap_or_else(|| key.to_string());
        let report = rollback_package(
            &self.dir,
            &id,
            to,
            &RollbackOptions {
                backup_keep,
                require_signature,
                pubkey: None,
                signature_config: None,
            },
        )?;
        self.refresh_after_install(&id);
        eprintln!(
            "[plugin-v3] rollback {} {} -> {}",
            report.id, report.from_version, report.to_version
        );
        for warning in &report.warnings {
            eprintln!("[plugin-v3] rollback warning: {warning}");
        }
        Ok(report)
    }

    /// 安装/回滚后刷新 store：已知 id 走 reload（保留启用/确认状态）；
    /// 新 id 重新解析目录并重建索引。被覆盖目录里的旧 id 会停止 supervisor。
    fn refresh_after_install(&mut self, id: &str) {
        if let Some(position) = self.index.get(id).copied() {
            if self.state.is_enabled(id) {
                if let Err(error) = self.reload_plugin(id) {
                    eprintln!("[plugin-v3] {id}: 安装后 reload 失败: {error}");
                }
            } else {
                // 未启用插件没有运行中的 supervisor：只重解析条目，不触发
                // “权限需要重新确认”的 reload 通知（首次 Enable 时仍会确认）。
                let dir = self.entries[position].dir.clone();
                self.entries[position] = parse_package_dir(&dir);
                self.rebuild_index();
                self.bump_runtime();
            }
            return;
        }
        let target = self.dir.join(id);
        let removed: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| entry.dir == target)
            .filter_map(|entry| entry.v3_key().map(str::to_string))
            .collect();
        if !removed.is_empty() {
            self.entries.retain(|entry| entry.dir != target);
            for removed_id in removed {
                if let Some(handle) = self.supervisors.remove(&removed_id) {
                    handle.shutdown();
                }
            }
        }
        self.entries.push(parse_package_dir(&target));
        self.entries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.key.cmp(&b.key))
        });
        self.rebuild_index();
        if let Some(position) = self.index.get(id).copied() {
            if self.entries[position].supported {
                if let Some(manifest) = self.entries[position].manifest.clone() {
                    if self.state.is_enabled(&manifest.id)
                        && self
                            .state
                            .approved_for(&manifest.id, &manifest.permission_fingerprint())
                    {
                        if let Err(error) = self.start_supervisor_at(position) {
                            eprintln!(
                                "[plugin-v3] {}: 安装后自动启动失败: {error}",
                                manifest.id
                            );
                        }
                    }
                }
            }
        }
        self.bump_runtime();
    }

    pub fn open_folder(&self, key: &str) -> Result<PathBuf, String> {
        let position = self
            .entry_index(key)
            .ok_or_else(|| format!("插件不存在: {key}"))?;
        let dir = self.entries[position].dir.clone();
        #[cfg(target_os = "linux")]
        let mut command = {
            let mut command = std::process::Command::new("xdg-open");
            command.arg(&dir);
            command
        };
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut command = std::process::Command::new("open");
            command.arg(&dir);
            command
        };
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut command = std::process::Command::new("explorer");
            command.arg(&dir);
            command
        };
        command
            .spawn()
            .map_err(|e| format!("无法打开 {}: {e}", dir.display()))?;
        Ok(dir)
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (position, entry) in self.entries.iter().enumerate() {
            self.index.insert(entry.key.clone(), position);
        }
    }

    fn persist_state(&mut self) {
        match save_state(&self.state_path, &self.state) {
            Ok(()) => self.state_error = None,
            Err(error) => {
                eprintln!("[plugin-v3] 保存状态失败: {error}");
                self.state_error = Some(error);
            },
        }
    }

    // ------------------------------------------------------------ 命令/事件

    fn intern(&mut self, value: &str) -> &'static str {
        if let Some(interned) = self.interns.get(value) {
            return interned;
        }
        let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
        self.interns.insert(value.to_string(), leaked);
        leaked
    }

    /// Palette 可用的插件命令（仅 enabled + 已确认权限 + `when` 成立）。字符串
    /// interning 只为兼容现有 `&'static str` 命令表；同一 id 重复 reload 不会再次泄漏。
    pub fn palette_commands(
        &mut self,
        query: &str,
        ctx: &CommandContext,
    ) -> Vec<(&'static str, String)> {
        let query = query.trim().to_lowercase();
        let mut found: Vec<(String, String)> = Vec::new();
        for entry in &self.entries {
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            if !entry.supported
                || !self.state.is_enabled(&manifest.id)
                || !self
                    .state
                    .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                continue;
            }
            for command in &manifest.commands {
                if !command_when_allows(command, ctx) {
                    continue;
                }
                let title = command.title.clone();
                let description = command.description.clone();
                let full_id = manifest.full_command_id(&command.id);
                if query.is_empty()
                    || title.to_lowercase().contains(&query)
                    || description.to_lowercase().contains(&query)
                    || full_id.to_lowercase().contains(&query)
                    || manifest.name.to_lowercase().contains(&query)
                {
                    found.push((full_id, title));
                }
            }
        }
        found.sort_by_key(|(_, title)| title.to_lowercase());
        found.truncate(20);
        found
            .into_iter()
            .map(|(id, title)| (self.intern(&id), title))
            .collect()
    }

    /// 终端右键菜单装饰项：enabled + 权限已确认且 `[[commands]] menu = true`。
    ///
    /// 与 [`Self::palette_commands`] 同一条命令通道：宿主把完整 ID 放进菜单，
    /// 点击后走 `run_command` → [`Self::spawn_command_for`]。插件只声明
    /// “加一项”，菜单位置/样式由宿主决定。
    pub fn menu_commands(&mut self) -> Vec<(&'static str, String)> {
        let mut found: Vec<(String, String)> = Vec::new();
        for entry in &self.entries {
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            if !entry.supported
                || !self.state.is_enabled(&manifest.id)
                || !self
                    .state
                    .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                continue;
            }
            for command in &manifest.commands {
                if !command.menu {
                    continue;
                }
                found.push((
                    manifest.full_command_id(&command.id),
                    command.title.clone(),
                ));
            }
        }
        found.sort_by_key(|(_, title)| title.to_lowercase());
        found.truncate(12);
        found
            .into_iter()
            .map(|(id, title)| (self.intern(&id), title))
            .collect()
    }


    /// 已启用 + 已确认权限的插件命令快照（Hotkeys 页与按键分发用）。
    ///
    /// Disable/Reload 后再次调用即返回新集合；这里不保留任何注册表缓存。
    pub fn command_refs(&self) -> Vec<PluginCommandRef> {
        let mut refs: Vec<PluginCommandRef> = Vec::new();
        for entry in &self.entries {
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            if !entry.supported
                || !self.state.is_enabled(&manifest.id)
                || !self
                    .state
                    .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                continue;
            }
            for command in &manifest.commands {
                let keybinding = command.keybinding.trim();
                let keybinding = if keybinding.is_empty() {
                    None
                } else {
                    crate::commands::Shortcut::parse(keybinding)
                        .ok()
                        .and_then(|shortcut| shortcut.single_chord().map(|chord| chord.canonical()))
                };
                refs.push(PluginCommandRef {
                    full_id: manifest.full_command_id(&command.id),
                    plugin_id: manifest.id.clone(),
                    plugin_name: manifest.name.clone(),
                    title: command.title.clone(),
                    description: command.description.clone(),
                    keybinding,
                    when: command.when.trim().to_string(),
                });
            }
        }
        refs.sort_by(|a, b| {
            a.plugin_name
                .to_lowercase()
                .cmp(&b.plugin_name.to_lowercase())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.full_id.cmp(&b.full_id))
        });
        refs
    }

    /// 这条插件命令当前是否可执行（存在、启用、权限确认、`when` 成立）。
    pub fn command_allowed(&self, full_id: &str, ctx: &CommandContext) -> bool {
        self.find_command(full_id)
            .is_some_and(|(_, decl)| command_when_allows(&decl, ctx))
    }

    /// UI 运行命令：识别 → 发 supervisor → 独立线程等结果；立即返回。
    ///
    /// `window_id` 是发起命令的窗口；插件收到 `command.invoke.window_id` 后可用
    /// 它把 `terminal.read/write`、`ui.render`、`ui.notify` 路由回同一窗口。
    /// `None` 回退到宿主记录的 active window（旧调用点兼容）。
    pub fn spawn_command(&self, full_id: &str) -> bool {
        self.spawn_command_for(full_id, None)
    }

    /// 带窗口上下文运行插件命令。
    pub fn spawn_command_for(&self, full_id: &str, window_id: Option<u64>) -> bool {
        let Some((manifest, decl)) = self.find_command(full_id) else {
            return false;
        };
        let window_id = window_id.or_else(|| self.active_window());
        let plugin_id = manifest.id.clone();
        let plugin_name = manifest.name.clone();
        let command_id = decl.id.clone();
        let action = format!("command {command_id}");
        let Some(handle) = self.supervisors.get(&plugin_id) else {
            let message = "插件进程未运行（Enable/确认权限后再试）".to_string();
            push_notice(&self.shared, Notice {
                plugin_id: plugin_id.clone(),
                plugin_name,
                text: message.clone(),
                level: "error".to_string(),
                window_id,
            });
            record_activity(&self.shared, &plugin_id, &action, "unavailable", &message);
            return true;
        };
        if !handle.is_running() {
            let message = format!("插件状态为 {}，命令未执行", handle.phase().as_str());
            push_notice(&self.shared, Notice {
                plugin_id: plugin_id.clone(),
                plugin_name,
                text: message.clone(),
                level: "error".to_string(),
                window_id,
            });
            record_activity(&self.shared, &plugin_id, &action, "unavailable", &message);
            return true;
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        if handle
            .tx
            .send(SupervisorMsg::Command {
                command: command_id.clone(),
                args: Vec::new(),
                window_id,
                reply: reply_tx,
            })
            .is_err()
        {
            let message = "插件 supervisor 已退出".to_string();
            push_notice(&self.shared, Notice {
                plugin_id: plugin_id.clone(),
                plugin_name,
                text: message.clone(),
                level: "error".to_string(),
                window_id,
            });
            record_activity(&self.shared, &plugin_id, &action, "unavailable", &message);
            return true;
        }
        let shared = self.shared.clone();
        let wait = self.options.command_timeout + Duration::from_secs(2);
        let full_id = full_id.to_string();
        let plugin_for_thread = plugin_id.clone();
        let plugin_name_for_thread = plugin_name.clone();
        let action_for_thread = action.clone();
        std::thread::Builder::new()
            .name(format!("plugin-v3-cmd-{plugin_id}"))
            .spawn(move || {
                let outcome = reply_rx.recv_timeout(wait).unwrap_or_else(|_| {
                    CommandOutcome::Unavailable("等待插件响应超时".to_string())
                });
                let text = sanitize_text(outcome.text(), protocol::MAX_RESULT_TEXT_CHARS);
                record_activity(
                    &shared,
                    &plugin_for_thread,
                    &action_for_thread,
                    outcome.label(),
                    &text,
                );
                eprintln!(
                    "[plugin-v3] {full_id} {}: {text}",
                    outcome.label()
                );
                push_notice(&shared, Notice {
                    plugin_id: plugin_for_thread,
                    plugin_name: plugin_name_for_thread,
                    text: sanitize_text(&text, 300),
                    level: if outcome.ok() { "info".to_string() } else { "error".to_string() },
                    window_id,
                });
                notify_wake(&shared.wake);
            })
            .ok();
        true
    }

    fn find_command(&self, full_id: &str) -> Option<(Manifest, CommandDecl)> {
        for entry in &self.entries {
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            if !entry.supported
                || !self.state.is_enabled(&manifest.id)
                || !self
                    .state
                    .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                continue;
            }
            for command in &manifest.commands {
                if manifest.full_command_id(&command.id) == full_id {
                    return Some((manifest.clone(), command.clone()));
                }
            }
        }
        None
    }

    /// 后台分发事件；只发给 enabled + 已确认 + 正在运行的插件。
    pub fn spawn_event(&self, event: PluginEvent, payload: &Value) -> usize {
        let mut delivered = 0;
        for entry in &self.entries {
            let Some(manifest) = &entry.manifest else {
                continue;
            };
            if !entry.supported
                || !manifest.event_enabled(event)
                || !self.state.is_enabled(&manifest.id)
                || !self
                    .state
                    .approved_for(&manifest.id, &manifest.permission_fingerprint())
            {
                continue;
            }
            let Some(handle) = self.supervisors.get(&manifest.id) else {
                continue;
            };
            // Starting 也允许投递：消息会排在 handshake 之后，避免 startup 事件
            // 因插件启动慢而丢失。Restarting/Failed 不投递，避免把旧事件送给新实例。
            if !handle.is_running() && handle.phase() != Phase::Starting {
                continue;
            }
            if handle
                .tx
                .send(SupervisorMsg::Event {
                    event,
                    payload: payload.clone(),
                })
                .is_ok()
            {
                delivered += 1;
            }
        }
        if delivered > 0 {
            eprintln!(
                "[plugin-v3] 事件 {} 已投递（{} 个插件）",
                event.as_str(),
                delivered
            );
        }
        delivered
    }

    /// 请求插件渲染声明式 surface；结果/错误通过共享队列回 UI，不阻塞。
    pub fn request_surface(
        &self,
        plugin_id: &str,
        surface: &str,
        panel: Option<String>,
    ) -> Result<(), String> {
        self.request_surface_for(plugin_id, surface, panel, None)
    }

    /// 带窗口上下文请求 surface；`surface.render.window_id` 会回传插件，
    /// 返回的 `SurfaceUpdate` 也带同一 window_id。
    pub fn request_surface_for(
        &self,
        plugin_id: &str,
        surface: &str,
        panel: Option<String>,
        window_id: Option<u64>,
    ) -> Result<(), String> {
        let window_id = window_id.or_else(|| self.active_window());
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.v3_key() == Some(plugin_id))
            .ok_or_else(|| format!("插件不存在: {plugin_id}"))?;
        let manifest = entry
            .manifest
            .as_ref()
            .ok_or_else(|| format!("插件 {plugin_id} 不是 v3"))?;
        if !self.state.is_enabled(plugin_id)
            || !self.state.approved_for(plugin_id, &manifest.permission_fingerprint())
        {
            return Err("插件未启用或权限未确认".to_string());
        }
        let (surface, panel) = match surface {
            "settings" => {
                if !manifest.contributes.settings {
                    return Err("插件未声明 contributes.settings".to_string());
                }
                ("settings".to_string(), None)
            },
            "panel" => {
                let panel = panel
                    .or_else(|| manifest.contributes.panels.first().cloned())
                    .ok_or_else(|| "插件未声明 contributes.panels".to_string())?;
                if !manifest.contributes.panels.iter().any(|id| id == &panel) {
                    return Err(format!("panel {panel:?} 未声明"));
                }
                (format!("panel:{panel}"), Some(panel))
            },
            other => return Err(format!("未知 surface {other:?}")),
        };
        let handle = self
            .supervisors
            .get(plugin_id)
            .ok_or_else(|| "插件进程未运行".to_string())?;
        if !handle.is_running() {
            return Err(format!("插件状态为 {}，无法渲染 UI", handle.phase().as_str()));
        }
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        handle
            .tx
            .send(SupervisorMsg::Render {
                surface: surface.clone(),
                panel: panel.clone(),
                window_id,
                reply: reply_tx,
            })
            .map_err(|_| "插件 supervisor 已退出".to_string())?;
        let shared = self.shared.clone();
        let plugin_id = plugin_id.to_string();
        let plugin_name = manifest.name.clone();
        let surface_for_thread = surface.clone();
        let wait = self.options.surface_timeout + Duration::from_secs(1);
        std::thread::Builder::new()
            .name(format!("plugin-v3-ui-{plugin_id}"))
            .spawn(move || {
                let result = reply_rx.recv_timeout(wait).unwrap_or_else(|_| {
                    Err("等待 surface.render 超时".to_string())
                });
                match result {
                    Ok(doc) => {
                        if let Ok(mut surfaces) = shared.surfaces.lock() {
                            if surfaces.len() >= 32 {
                                surfaces.pop_front();
                            }
                            surfaces.push_back(SurfaceUpdate {
                                plugin_id: plugin_id.clone(),
                                plugin_name,
                                surface: surface_for_thread,
                                panel,
                                doc,
                                window_id,
                                pushed: false,
                            });
                        }
                        notify_wake(&shared.wake);
                    },
                    Err(error) => {
                        push_notice(&shared, Notice {
                            plugin_id: plugin_id.clone(),
                            plugin_name,
                            text: format!("UI render failed: {}", sanitize_text(&error, 300)),
                            level: "error".to_string(),
                            window_id,
                        });
                    },
                }
            })
            .ok();
        Ok(())
    }

    /// UI 控件动作 -> 插件 `ui.action` notification；不等待回复。
    pub fn send_ui_action(&self, plugin_id: &str, surface: &str, panel: Option<String>, element: &str, action: &str, value: Option<String>) -> bool {
        self.send_ui_action_for(plugin_id, surface, panel, None, element, action, value)
    }

    /// 带窗口上下文发送 UI 动作；插件可据此只更新该窗口的面板。
    pub fn send_ui_action_for(
        &self,
        plugin_id: &str,
        surface: &str,
        panel: Option<String>,
        window_id: Option<u64>,
        element: &str,
        action: &str,
        value: Option<String>,
    ) -> bool {
        let Some(handle) = self.supervisors.get(plugin_id) else {
            return false;
        };
        if !handle.is_running() {
            return false;
        }
        handle
            .tx
            .send(SupervisorMsg::Action {
                surface: surface.to_string(),
                panel,
                window_id: window_id.or_else(|| self.active_window()),
                element: element.to_string(),
                action: action.to_string(),
                value,
            })
            .is_ok()
    }

    /// 启用了且权限已确认的插件主题（启动时并入主题列表；不做运行时换主题）。
    pub fn theme_contributions(&self) -> Vec<(String, PathBuf)> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let manifest = entry.manifest.as_ref()?;
                if !entry.supported
                    || !self.state.is_enabled(&manifest.id)
                    || !self
                        .state
                        .approved_for(&manifest.id, &manifest.permission_fingerprint())
                {
                    return None;
                }
                entry
                    .theme_path
                    .clone()
                    .map(|path| (manifest.name.clone(), path))
            })
            .collect()
    }

    /// 诊断日志用：unsupported/invalid 目录与原因。
    pub fn skipped(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .filter(|entry| !entry.supported)
            .map(|entry| (entry.dir.display().to_string(), entry.reason.clone()))
            .collect()
    }
}

/// 目标窗口匹配：显式 id 必须精确命中；缺省（None）落到 active 窗口。
///
/// 两个窗口的轮询都会调用 `take_*_for`；不匹配的条目必须留在共享队列里。
pub fn window_matches_target(target: Option<u64>, window_id: u64, active: Option<u64>) -> bool {
    match target {
        Some(id) => id == window_id,
        None => active == Some(window_id),
    }
}

fn take_matching<T>(
    queue: &Arc<Mutex<VecDeque<T>>>,
    predicate: impl Fn(&T) -> bool,
) -> Vec<T> {
    let Ok(mut queue) = queue.lock() else {
        return Vec::new();
    };
    let mut taken = Vec::new();
    let mut kept = VecDeque::with_capacity(queue.len());
    for item in queue.drain(..) {
        if predicate(&item) {
            taken.push(item);
        } else {
            kept.push_back(item);
        }
    }
    *queue = kept;
    taken
}

impl Drop for PluginStore {
    fn drop(&mut self) {
        self.shutdown_processes();
    }
}


// ---------------------------------------------------------------- 事件 payload

/// startup 事件不带任何环境/cwd/凭据信息。
pub fn startup_payload() -> Value {
    json!({})
}

/// tab-open 只带清洗后的标题和有限类型。
pub fn tab_open_payload(title: &str, kind: &str) -> Value {
    json!({
        "title": sanitize_text(title, 200),
        "kind": sanitize_tab_kind(kind),
    })
}

pub fn tab_close_payload(title: &str) -> Value {
    json!({ "title": sanitize_text(title, 200) })
}

/// pane-focus 只带不透明 pane id，不带标题/cwd/内容。
pub fn pane_focus_payload(pane: u64) -> Value {
    json!({ "pane": pane })
}

fn sanitize_tab_kind(kind: &str) -> String {
    let kind = kind.trim();
    if !kind.is_empty()
        && kind.len() <= 16
        && kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        kind.to_string()
    } else {
        "local".to_string()
    }
}
