// SPDX-License-Identifier: GPL-3.0-only
//! 插件清单与外部进程宿主（MVP）。
//!
//! 支持两种清单格式，互不替换：
//!
//! 1. **旧扁平清单**：`<plugins>/*.toml`（`id`/`name`/`version`/`description`/`enabled`）。
//!    仅发现、校验、展示，**不执行**；保留给旧配置与旧版设置页。
//! 2. **v1 目录插件**：`<plugins>/<name>/plugin.toml`，可声明 `[[commands]]` 与
//!    `events = ["startup"|"tab-open"|"tab-close"]`。启用后命令以 `plugin.<name>.<id>`
//!    注册进命令表；命令与事件都以后台线程 spawn `entry`，通过 stdin 发一行 JSON，
//!    stdout/stderr 写日志，超时 kill；最近 8 条执行摘要进设置页「最近插件活动」。
//!
//! 安全边界：插件是**普通子进程**，拥有当前用户全部权限，没有沙箱/权限模型；
//! `entry` 只允许插件目录内的相对路径（拒绝绝对路径、`..`、符号链接逃逸）。
//! 事件 payload 只带 Tab 标题/类型，不含 cwd、命令行、环境变量或凭据。
//! 详见 `docs/PLUGIN_API.md`。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use tervo_core::WakeSignal;

use crate::commands::{self, Command, Registry, Shortcut};

/// 宿主实现的插件 API 版本；清单里的 `api` 必须精确等于它。
pub const PLUGIN_API_VERSION: u32 = 1;

/// 单条插件命令的执行超时（MVP 固定值，不开放配置）。
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// 单次事件钩子的执行超时；与命令一致，避免坏插件卡住后台线程。
pub const EVENT_TIMEOUT: Duration = COMMAND_TIMEOUT;

/// 设置页「最近插件活动」保留/展示的条数（新→旧）。
pub const ACTIVITY_CAPACITY: usize = 8;

/// 活动列表里单条输出摘要的最大字符数（按字符，不按字节）。
pub const MAX_ACTIVITY_SUMMARY_CHARS: usize = 80;

/// 事件 payload 里标题的最大字符数（防止超长标题塞满插件 stdin/日志）。
pub const MAX_EVENT_TITLE_CHARS: usize = 200;

/// stdout/stderr 各最多保留的字节数，防止坏插件撑爆内存/日志。
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

/// 日志里单条流最多展示的字节数。
const MAX_LOG_BYTES: usize = 8 * 1024;

/// 宿主支持的生命周期事件钩子。
///
/// 清单里用字符串声明（`events = ["startup", "tab-open"]`）；宿主匹配到对应时机时，
/// 以后台线程执行插件 entry，并向 stdin 写一行
/// `{"event":"<name>","payload":{...}}`。每次事件都是新进程，不等插件返回。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginEvent {
    Startup,
    TabOpen,
    TabClose,
}

impl PluginEvent {
    /// 全部支持的事件（文档/校验/测试共用）。
    pub const ALL: [PluginEvent; 3] =
        [PluginEvent::Startup, PluginEvent::TabOpen, PluginEvent::TabClose];

    /// 协议里的稳定字符串（也是清单里的写法）。
    pub fn as_str(self) -> &'static str {
        match self {
            PluginEvent::Startup => "startup",
            PluginEvent::TabOpen => "tab-open",
            PluginEvent::TabClose => "tab-close",
        }
    }

    /// 解析清单/测试里的字符串；未知值返回 None。
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == value.trim())
    }
}

/// 一份旧扁平插件清单（`plugins/*.toml`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginManifest {
    /// 稳定 ID（唯一，建议反向域名）。
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    /// 旧清单里的未知字段原样保留；新格式不依赖它。
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            version: String::new(),
            description: String::new(),
            enabled: true,
            extra: toml::Table::new(),
        }
    }
}

impl PluginManifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("插件 id 不能为空".into());
        }
        if self.name.trim().is_empty() {
            return Err(format!("插件 {} 缺少 name", self.id));
        }
        if self.version.trim().is_empty() {
            return Err(format!("插件 {} 缺少 version", self.id));
        }
        Ok(())
    }

    /// 设置页展示用的一行。
    pub fn display_line(&self) -> String {
        let state = if self.enabled { "enabled" } else { "disabled" };
        if self.description.trim().is_empty() {
            format!("{} v{} — {state}", self.name, self.version)
        } else {
            format!("{} v{} — {state} — {}", self.name, self.version, self.description)
        }
    }
}

/// v1 目录清单里的一条命令声明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandDecl {
    /// 插件内唯一 ID；注册到命令表时变成 `plugin.<name>.<id>`。
    pub id: String,
    pub title: String,
    /// 可选默认快捷键（`ctrl+alt+g` / `f9`），必须是单个可绑定组合。
    #[serde(default)]
    pub key: Option<String>,
}

fn default_enabled() -> bool {
    true
}

/// v1 目录插件清单：`<plugins>/<name>/plugin.toml`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    /// 必须等于 [`PLUGIN_API_VERSION`]。
    pub api: u32,
    /// 相对插件目录的可执行文件；禁止绝对路径、`..` 与符号链接逃逸。
    pub entry: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub commands: Vec<PluginCommandDecl>,
    /// 可选事件订阅：`startup` / `tab-open` / `tab-close`；未知值整份清单跳过。
    #[serde(default)]
    pub events: Vec<String>,
}

impl PackageManifest {
    /// 不访问文件系统的字段校验；`entry` 是否存在/是否逃逸在加载时另行检查。
    pub fn validate(&self) -> Result<(), String> {
        validate_plugin_name(&self.name)?;
        if self.version.trim().is_empty() {
            return Err(format!("插件 {} 缺少 version", self.name));
        }
        if self.api != PLUGIN_API_VERSION {
            return Err(format!(
                "不支持的 api {}（宿主只支持 api={PLUGIN_API_VERSION}）",
                self.api
            ));
        }
        entry_path(&self.entry)?;
        for (i, command) in self.commands.iter().enumerate() {
            validate_command_id(&command.id)
                .map_err(|e| format!("插件 {} 的命令 #{} 非法: {e}", self.name, i + 1))?;
            if command.title.trim().is_empty() {
                return Err(format!("插件 {} 的命令 {} 缺少 title", self.name, command.id));
            }
            if self.commands[..i].iter().any(|prev| prev.id == command.id) {
                return Err(format!("插件 {} 内命令 id 重复: {}", self.name, command.id));
            }
            if let Some(key) = &command.key {
                let shortcut = Shortcut::parse(key)
                    .map_err(|e| format!("插件 {} 命令 {} 的 key 非法: {e}", self.name, command.id))?;
                if shortcut.chords.len() != 1 || !shortcut.chords[0].is_bindable() {
                    return Err(format!(
                        "插件 {} 命令 {} 的 key 必须是单个可绑定组合（Ctrl/Alt/Meta+键或 F1–F24）",
                        self.name, command.id
                    ));
                }
            }
        }
        for (i, event) in self.events.iter().enumerate() {
            let Some(parsed) = PluginEvent::parse(event) else {
                return Err(format!(
                    "插件 {} 的 events[{}] 不支持: {event:?}（可选 startup/tab-open/tab-close）",
                    self.name,
                    i + 1
                ));
            };
            if self.events[..i].iter().any(|prev| PluginEvent::parse(prev) == Some(parsed)) {
                return Err(format!("插件 {} 的 event 重复: {}", self.name, event.trim()));
            }
        }
        Ok(())
    }

    /// 清单是否声明了某个事件（不检查 `enabled`）。
    pub fn declares_event(&self, event: PluginEvent) -> bool {
        self.events.iter().any(|name| PluginEvent::parse(name) == Some(event))
    }
}

fn validate_plugin_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("插件 name 不能为空".into());
    }
    if name.len() > 64 {
        return Err(format!("插件 name 太长（最多 64 字节）: {name}"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("插件 name 只能包含字母/数字/-/_（同时用作命令命名空间）: {name}"));
    }
    Ok(())
}

fn validate_command_id(id: &str) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("命令 id 不能为空".into());
    }
    if id.len() > 64 {
        return Err(format!("命令 id 太长（最多 64 字节）: {id}"));
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!("命令 id 必须以小写字母或数字开头: {id}"));
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("命令 id 只能包含小写字母/数字/-/_/. : {id}"));
    }
    Ok(())
}

/// 校验 `entry` 是插件目录内的相对路径（不访问文件系统）。
fn entry_path(entry: &str) -> Result<&Path, String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err("entry 不能为空".into());
    }
    let path = Path::new(entry);
    if path.is_absolute() {
        return Err(format!("entry 必须是相对路径，不能是绝对路径: {entry}"));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {},
            Component::ParentDir => return Err(format!("entry 不能包含 ..: {entry}")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("entry 必须是相对路径: {entry}"));
            },
        }
    }
    Ok(path)
}

/// 解析并校验 entry 的真实路径：必须存在、是普通文件、canonical 后仍在插件目录内。
fn resolve_entry(plugin_dir: &Path, entry: &str) -> Result<PathBuf, String> {
    let relative = entry_path(entry)?;
    let canonical_dir = std::fs::canonicalize(plugin_dir)
        .map_err(|e| format!("插件目录不可访问 {}: {e}", plugin_dir.display()))?;
    let joined = canonical_dir.join(relative);
    let canonical_entry = std::fs::canonicalize(&joined)
        .map_err(|e| format!("entry 不存在或不可访问 {}: {e}", joined.display()))?;
    if !canonical_entry.starts_with(&canonical_dir) {
        return Err(format!("entry 解析后逃出插件目录: {}", canonical_entry.display()));
    }
    if !canonical_entry.is_file() {
        return Err(format!("entry 不是普通文件: {}", canonical_entry.display()));
    }
    Ok(canonical_entry)
}

/// 一个已加载的 v1 目录插件。
#[derive(Clone, Debug)]
pub struct PluginPackage {
    pub manifest: PackageManifest,
    /// 插件目录（canonical；执行时作为 cwd）。
    pub dir: PathBuf,
    /// 已解析的 entry 绝对路径（canonical）。
    pub entry: PathBuf,
}

impl PluginPackage {
    pub fn command_count(&self) -> usize {
        self.manifest.commands.len()
    }

    /// 命令表里的完整 ID：`plugin.<name>.<id>`。
    pub fn full_command_id(&self, command_id: &str) -> String {
        format!("plugin.{}.{}", self.manifest.name, command_id)
    }

    /// 设置页展示用的一行：名称/版本/状态/API/命令数/说明。
    pub fn display_line(&self) -> String {
        let state = if self.manifest.enabled { "enabled" } else { "disabled" };
        let mut line = format!(
            "{} v{} — {state} · api {} · {} command(s)",
            self.manifest.name,
            self.manifest.version,
            self.manifest.api,
            self.command_count()
        );
        if !self.manifest.events.is_empty() {
            line.push_str(&format!(" · {} event(s)", self.manifest.events.len()));
        }
        if !self.manifest.description.trim().is_empty() {
            line.push_str(" — ");
            line.push_str(self.manifest.description.trim());
        }
        line
    }
}

/// 发现结果。
#[derive(Clone, Debug, Default)]
pub struct PluginStore {
    /// 旧扁平清单（`plugins/*.toml`），只展示、不执行。
    pub plugins: Vec<PluginManifest>,
    /// v1 目录插件（`plugins/<name>/plugin.toml`），可执行。
    pub packages: Vec<PluginPackage>,
    /// 加载过程中被跳过的文件/目录（路径 + 原因），设置页可显示数量。
    pub skipped: Vec<(String, String)>,
    /// 最近命令/事件执行摘要；后台线程写，UI 轮询读取（互不阻塞）。
    activity: Arc<Mutex<ActivityLog>>,
}

/// 一条「最近插件活动」记录。
///
/// 只放设置页展示所需的摘要：时间/插件/动作/结果/输出首行。
/// 不收集环境变量、stdin、密码或完整输出（完整输出仍只写日志）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginActivity {
    /// 本地时间 `HH:MM:SS`（测试可构造任意字符串）。
    pub timestamp: String,
    pub plugin: String,
    /// 动作：`command <id>` 或 `event <name>`。
    pub action: String,
    /// 结果：`exit=<code>` / `timeout` / `signal` / `spawn-error`。
    pub outcome: String,
    /// stdout（优先）或 stderr 的首个非空行；已去控制字符并截断。
    pub summary: String,
}

impl PluginActivity {
    pub fn new(
        timestamp: impl Into<String>,
        plugin: impl Into<String>,
        action: impl Into<String>,
        outcome: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: timestamp.into(),
            plugin: plugin.into(),
            action: action.into(),
            outcome: outcome.into(),
            summary: summary.into(),
        }
    }

    fn now(plugin: &str, action: &str, outcome: &str, summary: &str) -> Self {
        Self::new(now_hms(), plugin, action, outcome, summary)
    }

    /// 从一次执行结果生成活动记录（命令与事件共用）。
    pub fn from_report(plugin: &str, action: &str, report: &ExecutionReport) -> Self {
        let outcome = if report.timed_out {
            "timeout".to_string()
        } else if let Some(code) = report.exit_code {
            format!("exit={code}")
        } else {
            "signal".to_string()
        };
        Self::now(plugin, action, &outcome, &activity_summary(report))
    }

    /// 进程都没起来（entry 丢失/权限不足/线程失败）时的记录。
    pub fn from_error(plugin: &str, action: &str, error: &str) -> Self {
        Self::now(plugin, action, "spawn-error", &sanitize_activity_text(error))
    }

    /// 设置页列表中的一行：`12:34:56 · hello · command greet · exit=0 · 输出首行`。
    pub fn display_line(&self) -> String {
        let mut line = String::new();
        if !self.timestamp.is_empty() {
            line.push_str(&self.timestamp);
            line.push_str(" · ");
        }
        line.push_str(&self.plugin);
        line.push_str(" · ");
        line.push_str(&self.action);
        line.push_str(" · ");
        line.push_str(&self.outcome);
        if !self.summary.is_empty() {
            line.push_str(" · ");
            line.push_str(&self.summary);
        }
        line
    }
}

/// 有界的最近活动环形缓冲（新记录追加在尾部，展示时反向）。
#[derive(Clone, Debug, Default)]
pub struct ActivityLog {
    records: VecDeque<PluginActivity>,
    revision: u64,
    /// 活动变化唤醒 UI（容量 1，合并）；后台线程 push 后通知，UI 立即刷新。
    wake: Option<WakeSignal>,
}

impl ActivityLog {
    /// 安装唤醒信号；`PluginStore` 在启动时注入，测试可用默认空值。
    pub fn set_wake(&mut self, wake: WakeSignal) {
        self.wake = Some(wake);
    }

    pub fn push(&mut self, record: PluginActivity) {
        if self.records.len() >= ACTIVITY_CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(record);
        self.revision = self.revision.wrapping_add(1);
        // 非阻塞：容量 1，连续 push 只合并成一次 UI 唤醒；实际记录仍逐条保留。
        if let Some(wake) = &self.wake {
            wake.notify();
        }
    }

    #[allow(dead_code)] // 单测断言容量/空表；生产只读 lines()
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    /// 每次 push 递增；UI 只在 revision 变化时刷新 Slint model。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 新 → 旧顺序的展示行。
    pub fn lines(&self) -> Vec<String> {
        self.records.iter().rev().map(PluginActivity::display_line).collect()
    }
}

/// 本地时间 `HH:MM:SS`（不引入时间库；失败时返回空串，调用方仍能展示记录）。
fn now_hms() -> String {
    #[cfg(unix)]
    unsafe {
        let mut now: libc::time_t = 0;
        libc::time(&mut now);
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
    #[cfg(not(unix))]
    {
        String::new()
    }
}

fn sanitize_activity_text(text: &str) -> String {
    crate::activity::sanitize_notification_text(text, MAX_ACTIVITY_SUMMARY_CHARS)
}

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// 输出摘要：stdout 首行优先，其次 stderr；去控制字符并截断。
fn activity_summary(report: &ExecutionReport) -> String {
    let raw =
        first_nonempty_line(&report.stdout).or_else(|| first_nonempty_line(&report.stderr)).unwrap_or("");
    sanitize_activity_text(raw)
}

fn record_activity(log: &Arc<Mutex<ActivityLog>>, record: PluginActivity) {
    match log.lock() {
        Ok(mut log) => log.push(record),
        Err(_) => eprintln!("[plugin] 活动记录锁被污染，丢弃一条记录"),
    }
}


impl PluginStore {
    pub fn dir() -> PathBuf {
        if let Ok(p) = std::env::var("TERVO_PLUGINS") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("tervo").join("plugins")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::dir())
    }

    pub fn load_from(dir: &Path) -> Self {
        let mut store = PluginStore::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return store; // 目录不存在 = 没有插件
        };
        let mut flat_paths: Vec<PathBuf> = Vec::new();
        let mut package_dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() && path.extension().map(|e| e == "toml").unwrap_or(false) {
                flat_paths.push(path);
            } else if path.is_dir() && path.join("plugin.toml").is_file() {
                // 只把带 plugin.toml 的子目录当 v1 插件，避免把任意目录当坏清单。
                package_dirs.push(path);
            }
        }
        flat_paths.sort();
        package_dirs.sort();

        for path in flat_paths {
            store.load_flat(&path);
        }
        for path in package_dirs {
            store.load_package_dir(&path);
        }

        store.plugins.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        store.packages.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        store.skipped.sort_by(|a, b| a.0.cmp(&b.0));
        store
    }

    fn load_flat(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.skipped.push((path.display().to_string(), format!("读取失败: {e}")));
                return;
            },
        };
        let manifest: PluginManifest = match toml::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                self.skipped.push((path.display().to_string(), format!("解析失败: {e}")));
                return;
            },
        };
        if let Err(e) = manifest.validate() {
            self.skipped.push((path.display().to_string(), e));
            return;
        }
        if self.plugins.iter().any(|p| p.id == manifest.id) {
            self.skipped.push((path.display().to_string(), format!("重复 id {}", manifest.id)));
            return;
        }
        self.plugins.push(manifest);
    }

    fn load_package_dir(&mut self, dir: &Path) {
        let manifest_path = dir.join("plugin.toml");
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => {
                self.skipped
                    .push((manifest_path.display().to_string(), format!("读取失败: {e}")));
                return;
            },
        };
        let manifest: PackageManifest = match toml::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                self.skipped
                    .push((manifest_path.display().to_string(), format!("解析失败: {e}")));
                return;
            },
        };
        if let Err(e) = manifest.validate() {
            self.skipped.push((manifest_path.display().to_string(), e));
            return;
        }
        let canonical_dir = match std::fs::canonicalize(dir) {
            Ok(d) => d,
            Err(e) => {
                self.skipped.push((
                    manifest_path.display().to_string(),
                    format!("插件目录不可访问: {e}"),
                ));
                return;
            },
        };
        let entry = match resolve_entry(&canonical_dir, &manifest.entry) {
            Ok(entry) => entry,
            Err(e) => {
                self.skipped.push((manifest_path.display().to_string(), e));
                return;
            },
        };
        if self.packages.iter().any(|p| p.manifest.name == manifest.name) {
            self.skipped.push((
                manifest_path.display().to_string(),
                format!("重复插件 name {}", manifest.name),
            ));
            return;
        }
        self.packages.push(PluginPackage { manifest, dir: canonical_dir, entry });
    }

    /// 设置页列表：旧清单 + v1 目录插件，按名称排序。
    pub fn display_lines(&self) -> Vec<String> {
        let mut rows: Vec<(String, String)> = self
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.display_line()))
            .chain(
                self.packages
                    .iter()
                    .map(|p| (p.manifest.name.clone(), p.display_line())),
            )
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        rows.into_iter().map(|(_, line)| line).collect()
    }

    pub fn total_count(&self) -> usize {
        self.plugins.len() + self.packages.len()
    }

    pub fn enabled_count(&self) -> usize {
        self.plugins.iter().filter(|p| p.enabled).count()
            + self.packages.iter().filter(|p| p.manifest.enabled).count()
    }

    /// 已启用插件声明的命令总数（实际注册数可能因 ID 冲突略少）。
    pub fn enabled_command_count(&self) -> usize {
        self.packages
            .iter()
            .filter(|p| p.manifest.enabled)
            .map(PluginPackage::command_count)
            .sum()
    }

    /// 命令表扩展点：把启用插件声明的命令合并进 [`Registry`]，返回成功注册数。
    ///
    /// [`Command`] 的 ID/标题/快捷键使用 `&'static str`，而插件命令是运行时字符串。
    /// 这里在启动期一次性 `Box::leak`（每个命令有界、只注册一次），避免为了插件把
    /// 整个命令表改成 `Cow`/`String` 的大重构；禁用/重复注册不会重复 leak。
    pub fn register_commands(&self, registry: &mut Registry) -> usize {
        let mut registered = 0;
        for package in self.packages.iter().filter(|p| p.manifest.enabled) {
            for decl in &package.manifest.commands {
                let full_id = package.full_command_id(&decl.id);
                let title = format!("{} (plugin: {})", decl.title, package.manifest.name);
                let shortcut: Option<&'static str> = decl
                    .key
                    .as_deref()
                    .map(|key| &*Box::leak(key.to_string().into_boxed_str()));
                let command = Command {
                    id: &*Box::leak(full_id.into_boxed_str()),
                    title: &*Box::leak(title.into_boxed_str()),
                    category: commands::Category::Plugin,
                    default_shortcut: shortcut,
                };
                match registry.register(command) {
                    Ok(()) => registered += 1,
                    Err(id) => eprintln!("[plugin] 命令注册冲突，已跳过: {id}"),
                }
            }
        }
        registered
    }

    /// 找到一条已启用插件命令（用于执行/诊断）。
    pub fn find_command(&self, full_id: &str) -> Option<(&PluginPackage, &PluginCommandDecl)> {
        self.packages
            .iter()
            .filter(|p| p.manifest.enabled)
            .find_map(|package| {
                package
                    .manifest
                    .commands
                    .iter()
                    .find(|decl| package.full_command_id(&decl.id) == full_id)
                    .map(|decl| (package, decl))
            })
    }

    /// UI 读取最近活动：返回 (revision, 新→旧展示行)。
    ///
    /// 只读快照；后台线程 push 时短暂持锁，不会阻塞 UI 线程。
    pub fn activity_snapshot(&self) -> (u64, Vec<String>) {
        match self.activity.lock() {
            Ok(log) => (log.revision(), log.lines()),
            Err(_) => (0, Vec::new()),
        }
    }

    /// 插件活动写入后唤醒 UI（容量 1）。必须在事件循环启动前注入。
    pub fn set_activity_wake(&self, wake: WakeSignal) {
        if let Ok(mut log) = self.activity.lock() {
            log.set_wake(wake);
        }
    }

    /// 声明并启用了该事件的插件（顺序与 `packages` 一致）。
    pub fn event_targets(&self, event: PluginEvent) -> Vec<&PluginPackage> {
        self.packages
            .iter()
            .filter(|package| package.manifest.enabled && package.manifest.declares_event(event))
            .collect()
    }

    /// 在后台线程把一次事件分发给所有声明该事件的已启用插件。
    ///
    /// 返回已提交的插件数；每个插件一个线程，spawn 后立即返回，绝不阻塞 UI。
    /// 事件失败只写日志与活动列表，不影响主流程；禁用插件不会被选中。
    pub fn spawn_event(&self, event: PluginEvent, payload: serde_json::Value) -> usize {
        let mut spawned = 0;
        for package in self.event_targets(event) {
            let plugin_name = package.manifest.name.clone();
            let plugin_dir = package.dir.clone();
            let entry = package.entry.clone();
            let activity = self.activity.clone();
            let action = format!("event {}", event.as_str());
            let label = format!("{plugin_name}:{action}");
            let thread_name = format!("plugin-{plugin_name}-{}", event.as_str());
            let plugin_for_thread = plugin_name.clone();
            let action_for_thread = action.clone();
            let activity_for_thread = activity.clone();
            let thread_payload = payload.clone();
            let result = std::thread::Builder::new().name(thread_name).spawn(move || {
                match execute_event(
                    &plugin_for_thread,
                    &plugin_dir,
                    &entry,
                    event,
                    &thread_payload,
                    EVENT_TIMEOUT,
                ) {
                    Ok(report) => {
                        log_execution(&label, &report);
                        record_activity(
                            &activity_for_thread,
                            PluginActivity::from_report(
                                &plugin_for_thread,
                                &action_for_thread,
                                &report,
                            ),
                        );
                    },
                    Err(e) => {
                        eprintln!("[plugin] {plugin_for_thread}: 执行 {action_for_thread} 失败: {e}");
                        record_activity(
                            &activity_for_thread,
                            PluginActivity::from_error(
                                &plugin_for_thread,
                                &action_for_thread,
                                &e,
                            ),
                        );
                    },
                }
            });
            match result {
                Ok(_) => spawned += 1,
                Err(e) => {
                    eprintln!("[plugin] {plugin_name}: 无法创建事件线程 ({action}): {e}");
                    record_activity(
                        &activity,
                        PluginActivity::from_error(
                            &plugin_name,
                            &action,
                            &format!("无法创建事件线程: {e}"),
                        ),
                    );
                },
            }
        }
        spawned
    }

    /// 在后台线程执行一条插件命令。返回 false 表示不是已启用的插件命令。
    ///
    /// 绝不阻塞调用方（UI 线程）：spawn 后立即返回，stdout/stderr/退出码写日志，
    /// 同时把一条摘要放进「最近插件活动」。
    pub fn spawn_command(&self, full_id: &str) -> bool {
        let Some((package, decl)) = self.find_command(full_id) else {
            return false;
        };
        let plugin_name = package.manifest.name.clone();
        let plugin_dir = package.dir.clone();
        let entry = package.entry.clone();
        let command_id = decl.id.clone();
        let full_id = full_id.to_string();
        let error_label = full_id.clone();
        let action = format!("command {command_id}");
        let activity = self.activity.clone();
        let thread_name = format!("plugin-{plugin_name}");
        let spawned = std::thread::Builder::new().name(thread_name).spawn({
            let plugin_name = plugin_name.clone();
            let action = action.clone();
            let activity = activity.clone();
            move || {
                match execute(&plugin_name, &plugin_dir, &entry, &command_id, &[], COMMAND_TIMEOUT) {
                    Ok(report) => {
                        log_execution(&full_id, &report);
                        record_activity(
                            &activity,
                            PluginActivity::from_report(&plugin_name, &action, &report),
                        );
                    },
                    Err(e) => {
                        eprintln!("[plugin] {plugin_name}: 执行 {full_id} 失败: {e}");
                        record_activity(
                            &activity,
                            PluginActivity::from_error(&plugin_name, &action, &e),
                        );
                    },
                }
            }
        });
        match spawned {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[plugin] {error_label}: 无法创建执行线程: {e}");
                record_activity(
                    &activity,
                    PluginActivity::from_error(
                        &plugin_name,
                        &action,
                        &format!("无法创建执行线程: {e}"),
                    ),
                );
                false
            },
        }
    }
}

/// 一次插件执行的结果（完整输出只写日志；摘要进「最近插件活动」）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionReport {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
}

impl ExecutionReport {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// 启动事件 payload：当前不放任何环境/会话信息。
pub fn startup_payload() -> serde_json::Value {
    serde_json::json!({})
}

/// tab-open payload：只含标题与类型，不含 cwd/命令行/环境等敏感信息。
pub fn tab_open_payload(title: &str, kind: &str) -> serde_json::Value {
    serde_json::json!({
        "title": sanitize_event_title(title),
        "kind": sanitize_tab_kind(kind),
    })
}

/// tab-close payload：只含标题（关闭后不再提供 cwd/内容）。
pub fn tab_close_payload(title: &str) -> serde_json::Value {
    serde_json::json!({ "title": sanitize_event_title(title) })
}

fn sanitize_event_title(title: &str) -> String {
    crate::activity::sanitize_notification_text(title, MAX_EVENT_TITLE_CHARS)
}

/// kind 只允许短 ASCII 标识；未知/异常值回退 `local`，避免把任意字段原样传给插件。
fn sanitize_tab_kind(kind: &str) -> String {
    let kind = kind.trim();
    if !kind.is_empty()
        && kind.len() <= 16
        && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        kind.to_string()
    } else {
        "local".to_string()
    }
}

/// 事件协议行：`{"event":"tab-open","payload":{...}}\n`（单行 JSON）。
pub fn event_message(event: PluginEvent, payload: &serde_json::Value) -> String {
    serde_json::json!({
        "event": event.as_str(),
        "payload": payload,
    })
    .to_string()
        + "\n"
}

/// 执行一次插件 entry（同步，命令事件）。生产路径由 [`PluginStore::spawn_command`]
/// 放到后台线程；测试直接调用本函数，不需要启动 GUI。
pub(crate) fn execute(
    plugin_name: &str,
    plugin_dir: &Path,
    entry: &Path,
    command_id: &str,
    args: &[String],
    timeout: Duration,
) -> Result<ExecutionReport, String> {
    let message = serde_json::json!({
        "event": "command",
        "id": command_id,
        "args": args,
    })
    .to_string()
        + "\n";
    execute_message(plugin_name, plugin_dir, entry, &message, timeout)
}

/// 执行一次事件钩子（同步）。仅由 [`PluginStore::spawn_event`] 的后台线程与测试调用。
pub(crate) fn execute_event(
    plugin_name: &str,
    plugin_dir: &Path,
    entry: &Path,
    event: PluginEvent,
    payload: &serde_json::Value,
    timeout: Duration,
) -> Result<ExecutionReport, String> {
    let message = event_message(event, payload);
    execute_message(plugin_name, plugin_dir, entry, &message, timeout)
}

/// 同步执行 entry：写一行 JSON 到 stdin，并发捕获输出，超时 kill。
fn execute_message(
    plugin_name: &str,
    plugin_dir: &Path,
    entry: &Path,
    message: &str,
    timeout: Duration,
) -> Result<ExecutionReport, String> {
    // 刚写完的 entry 在部分文件系统上会短暂 ETXTBSY；重试几次比把测试/插件判死更稳。
    let mut child = None;
    for attempt in 0..5 {
        let spawned = ProcessCommand::new(entry)
            .current_dir(plugin_dir)
            .env("TERVO_PLUGIN_NAME", plugin_name)
            .env("TERVO_PLUGIN_API", PLUGIN_API_VERSION.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match spawned {
            Ok(spawned_child) => {
                child = Some(spawned_child);
                break;
            },
            Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                std::thread::sleep(Duration::from_millis(20));
            },
            Err(e) => return Err(format!("无法启动 {}: {e}", entry.display())),
        }
    }
    let mut child = child.ok_or_else(|| format!("无法启动 {}: 重试后仍失败", entry.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        // 写失败（插件提前退出）不是宿主错误；后续按退出码记录。
        let _ = stdin.write_all(message.as_bytes());
        // drop(stdin) 关闭管道，插件读到 EOF。
    }

    let stdout_pipe = child.stdout.take().ok_or_else(|| "stdout 管道不可用".to_string())?;
    let stderr_pipe = child.stderr.take().ok_or_else(|| "stderr 管道不可用".to_string())?;
    // 必须并发读两个管道，否则插件写满任一管道会阻塞，宿主却在等退出码 → 死锁。
    let stdout_thread = std::thread::spawn(move || read_capped(stdout_pipe, MAX_CAPTURE_BYTES));
    let stderr_thread = std::thread::spawn(move || read_capped(stderr_pipe, MAX_CAPTURE_BYTES));

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    timed_out = true;
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(10));
            },
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("等待插件进程失败: {e}"));
            },
        }
    };
    let elapsed = start.elapsed();

    let (stdout, stdout_truncated) =
        stdout_thread.join().unwrap_or_else(|_| (String::new(), true));
    let (stderr, stderr_truncated) =
        stderr_thread.join().unwrap_or_else(|_| (String::new(), true));

    Ok(ExecutionReport {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        elapsed,
    })
}

fn read_capped(mut reader: impl Read, cap: usize) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    truncated |= take < n;
                } else {
                    truncated = true;
                }
            },
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

fn clip_for_log(text: &str, max: usize) -> String {
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（已截断）", &text[..end])
}

fn log_execution(full_id: &str, report: &ExecutionReport) {
    let status = if report.timed_out {
        "超时，已 kill".to_string()
    } else {
        format!("exit={:?}", report.exit_code)
    };
    eprintln!(
        "[plugin] {full_id} {status} elapsed={}ms stdout={}B stderr={}B",
        report.elapsed.as_millis(),
        report.stdout.len(),
        report.stderr.len()
    );
    if !report.stdout.is_empty() {
        eprintln!("[plugin] {full_id} stdout:\n{}", clip_for_log(&report.stdout, MAX_LOG_BYTES));
    }
    if !report.stderr.is_empty() {
        eprintln!("[plugin] {full_id} stderr:\n{}", clip_for_log(&report.stderr, MAX_LOG_BYTES));
    }
    if report.stdout_truncated || report.stderr_truncated {
        eprintln!("[plugin] {full_id} 输出超过 {MAX_CAPTURE_BYTES} 字节，日志已截断");
    }
    if !report.success() {
        eprintln!("[plugin] {full_id} 执行失败（{status}）");
    }
}

/// 把插件清单放进命令表。显式的扩展点，主程序不该手工拼插件 Command。
pub fn register_plugins(registry: &mut Registry, store: &PluginStore) -> usize {
    store.register_commands(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-plug-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn write_package(root: &Path, name: &str, manifest: &str, script: Option<&str>) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
        if let Some(body) = script {
            write_script(&dir.join("run.sh"), body);
        }
        dir
    }

    const HELLO_MANIFEST: &str = r#"
name = "hello"
version = "1.2.3"
api = 1
entry = "run.sh"
description = "demo plugin"

[[commands]]
id = "greet"
title = "Greet"
key = "ctrl+alt+g"

[[commands]]
id = "second"
title = "Second"
"#;

    const EVENT_MANIFEST: &str = r#"
name = "events"
version = "0.1.0"
api = 1
entry = "run.sh"
events = ["startup", "tab-open", "tab-close"]

[[commands]]
id = "ping"
title = "Ping"
"#;

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cond() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    // ---------------------------------------------------------- 旧扁平清单

    #[test]
    fn loads_valid_manifests_and_skips_broken() {
        let dir = tmpdir("load");
        std::fs::write(
            dir.join("a.toml"),
            r#"
id = "org.example.a"
name = "Alpha"
version = "1.0.0"
description = "first"
enabled = true
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.toml"),
            r#"
id = "org.example.b"
name = "Beta"
version = "0.2.1"
enabled = false
"#,
        )
        .unwrap();
        std::fs::write(dir.join("broken.toml"), "id = [oops\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "not a manifest").unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 2);
        assert_eq!(store.packages.len(), 0);
        assert_eq!(store.skipped.len(), 1);
        assert_eq!(store.enabled_count(), 1);
        assert_eq!(store.plugins[0].name, "Alpha");
        assert!(store.plugins[0].display_line().contains("1.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_and_invalid_fields_are_rejected() {
        let dir = tmpdir("dup");
        std::fs::write(dir.join("a.toml"), "id = \"same\"\nname = \"A\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("b.toml"), "id = \"same\"\nname = \"B\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("c.toml"), "id = \"c\"\nname = \"\"\nversion = \"1\"\n").unwrap();
        std::fs::write(dir.join("d.toml"), "id = \"d\"\nname = \"D\"\n").unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 1);
        assert_eq!(store.plugins[0].name, "A");
        assert_eq!(store.skipped.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty_not_error() {
        let store = PluginStore::load_from(Path::new("/nonexistent/rt-plugins"));
        assert!(store.plugins.is_empty());
        assert!(store.packages.is_empty());
        assert!(store.skipped.is_empty());
    }

    #[test]
    fn unknown_fields_are_preserved() {
        let dir = tmpdir("extra");
        std::fs::write(
            dir.join("x.toml"),
            "id = \"x\"\nname = \"X\"\nversion = \"1\"\nentry = \"index.js\"\n",
        )
        .unwrap();
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 1);
        assert_eq!(
            store.plugins[0].extra.get("entry").and_then(|v| v.as_str()),
            Some("index.js")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------- v1 目录清单

    #[test]
    fn loads_v1_package_and_commands() {
        let dir = tmpdir("v1-load");
        write_package(&dir, "hello", HELLO_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 1);
        let package = &store.packages[0];
        assert_eq!(package.manifest.name, "hello");
        assert_eq!(package.manifest.version, "1.2.3");
        assert_eq!(package.manifest.api, 1);
        assert!(package.manifest.enabled);
        assert!(package.manifest.events.is_empty(), "未声明 events 时默认不订阅");
        assert_eq!(package.command_count(), 2);
        assert!(package.entry.is_file());
        assert_eq!(package.full_command_id("greet"), "plugin.hello.greet");
        let line = package.display_line();
        assert!(line.contains("hello v1.2.3"), "{line}");
        assert!(line.contains("enabled"), "{line}");
        assert!(line.contains("2 command(s)"), "{line}");
        assert_eq!(store.total_count(), 1);
        assert_eq!(store.enabled_command_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_and_v1_packages_coexist() {
        let dir = tmpdir("both");
        std::fs::write(dir.join("legacy.toml"), "id = \"old\"\nname = \"Old\"\nversion = \"0.1\"\n")
            .unwrap();
        write_package(&dir, "hello", HELLO_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.plugins.len(), 1);
        assert_eq!(store.packages.len(), 1);
        assert_eq!(store.display_lines().len(), 2);
        assert_eq!(store.total_count(), 2);
        assert_eq!(store.enabled_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_unsupported_api() {
        let dir = tmpdir("api");
        let manifest = HELLO_MANIFEST.replace("api = 1", "api = 2");
        write_package(&dir, "hello", &manifest, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.packages.is_empty());
        assert_eq!(store.skipped.len(), 1);
        assert!(store.skipped[0].1.contains("api 2"), "{:?}", store.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_missing_required_fields_and_bad_metadata() {
        for (tag, manifest, needle) in [
            ("no-entry", "name = \"hello\"\nversion = \"1\"\napi = 1\n", "entry"),
            ("no-name", "version = \"1\"\napi = 1\nentry = \"run.sh\"\n", "name"),
            ("no-version", "name = \"hello\"\napi = 1\nentry = \"run.sh\"\n", "version"),
            (
                "bad-name",
                "name = \"bad/name\"\nversion = \"1\"\napi = 1\nentry = \"run.sh\"\n",
                "name",
            ),
            (
                "empty-name",
                "name = \"\"\nversion = \"1\"\napi = 1\nentry = \"run.sh\"\n",
                "name",
            ),
        ] {
            let dir = tmpdir(&format!("fields-{tag}"));
            write_package(&dir, "hello", manifest, Some("#!/bin/sh\nexit 0\n"));
            let store = PluginStore::load_from(&dir);
            assert!(store.packages.is_empty(), "{tag}");
            assert_eq!(store.skipped.len(), 1, "{tag}");
            assert!(store.skipped[0].1.contains(needle), "{tag}: {:?}", store.skipped);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn rejects_entry_escape_absolute_parent_and_missing() {
        let cases = [
            ("abs", "entry = \"/bin/sh\"", "绝对路径"),
            ("parent", "entry = \"../outside.sh\"", ".."),
            ("missing", "entry = \"nope.sh\"", "不存在"),
        ];
        for (tag, entry_line, needle) in cases {
            let dir = tmpdir(&format!("entry-{tag}"));
            let manifest = HELLO_MANIFEST.replace("entry = \"run.sh\"", entry_line);
            write_package(&dir, "hello", &manifest, Some("#!/bin/sh\nexit 0\n"));
            let store = PluginStore::load_from(&dir);
            assert!(store.packages.is_empty(), "{tag}");
            assert_eq!(store.skipped.len(), 1, "{tag}: {:?}", store.skipped);
            assert!(store.skipped[0].1.contains(needle), "{tag}: {:?}", store.skipped);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tmpdir("symlink");
        let outside = tmpdir("symlink-outside");
        let target = outside.join("evil.sh");
        std::fs::write(&target, "#!/bin/sh\necho pwned\n").unwrap();
        let pkg = dir.join("hello");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("plugin.toml"), HELLO_MANIFEST).unwrap();
        symlink(&target, pkg.join("run.sh")).unwrap();
        let store = PluginStore::load_from(&dir);
        assert!(store.packages.is_empty());
        assert_eq!(store.skipped.len(), 1);
        assert!(store.skipped[0].1.contains("逃出插件目录"), "{:?}", store.skipped);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn rejects_duplicate_package_name_and_duplicate_command_ids() {
        let dir = tmpdir("pkg-dups");
        write_package(&dir, "a", HELLO_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        write_package(&dir, "b", HELLO_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.packages.len(), 1);
        assert_eq!(store.skipped.len(), 1);
        assert!(store.skipped[0].1.contains("重复插件 name"), "{:?}", store.skipped);
        let _ = std::fs::remove_dir_all(&dir);

        let dir = tmpdir("cmd-dups");
        let dup = HELLO_MANIFEST.replace("id = \"second\"", "id = \"greet\"");
        write_package(&dir, "hello", &dup, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.packages.is_empty());
        assert!(store.skipped[0].1.contains("命令 id 重复"), "{:?}", store.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_command_key() {
        for (tag, key) in [("plain", "a"), ("unknown", "ctrl+banana"), ("multi", "ctrl+k ctrl+t")] {
            let dir = tmpdir(&format!("key-{tag}"));
            let manifest = HELLO_MANIFEST.replace("key = \"ctrl+alt+g\"", &format!("key = \"{key}\""));
            write_package(&dir, "hello", &manifest, Some("#!/bin/sh\nexit 0\n"));
            let store = PluginStore::load_from(&dir);
            assert!(store.packages.is_empty(), "{tag}");
            assert!(store.skipped[0].1.contains("key"), "{tag}: {:?}", store.skipped);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ---------------------------------------------------------- 命令注册

    #[test]
    fn registers_namespaced_commands_and_skips_disabled() {
        let dir = tmpdir("register");
        write_package(&dir, "hello", HELLO_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        let off_manifest = HELLO_MANIFEST
            .replace("name = \"hello\"", "name = \"off\"")
            .replace(
                "description = \"demo plugin\"",
                "description = \"demo plugin\"\nenabled = false",
            );
        write_package(&dir, "off", &off_manifest, Some("#!/bin/sh\nexit 0\n"));

        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 2);
        assert_eq!(store.enabled_count(), 1);
        assert_eq!(store.enabled_command_count(), 2);

        let mut registry = commands::builtin();
        let before = registry.len();
        let n = register_plugins(&mut registry, &store);
        assert_eq!(n, 2);
        assert_eq!(registry.len(), before + 2);

        let cmd = registry.get("plugin.hello.greet").expect("命名空间命令必须注册");
        assert_eq!(cmd.title, "Greet (plugin: hello)");
        assert_eq!(cmd.category, commands::Category::Plugin);
        assert_eq!(cmd.category.label(), "Plugins");
        assert_eq!(cmd.default_shortcut, Some("ctrl+alt+g"));
        assert!(registry.get("plugin.hello.second").is_some());
        assert!(registry.get("plugin.off.greet").is_none());
        assert_eq!(registry.len(), before + 2);
        // Command Palette 走 Registry::search，插件命令必须能被搜到。
        let hits = registry.search("Greet");
        assert!(hits.iter().any(|c| c.id == "plugin.hello.greet"), "{hits:?}");
        assert_eq!(store.find_command("plugin.hello.greet").map(|(p, c)| (p.manifest.name.as_str(), c.id.as_str())), Some(("hello", "greet")));
        assert!(store.find_command("plugin.off.greet").is_none());
        assert!(!store.spawn_command("plugin.off.greet"));
        assert!(!store.spawn_command("not.a.command"));

        // 插件默认快捷键只匹配插件命令，且由输入层在内置键之后调用。
        let chord = commands::Chord::parse("ctrl+alt+g").unwrap();
        assert_eq!(registry.plugin_default_for_chord(&chord), Some("plugin.hello.greet"));
        assert_eq!(
            registry.plugin_default_for_chord(&commands::Chord::parse("ctrl+t").unwrap()),
            None
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------- 子进程宿主

    #[cfg(unix)]
    #[test]
    fn execute_captures_env_protocol_and_streams() {
        let dir = tmpdir("exec-ok");
        let script = r#"#!/bin/sh
printf 'name=%s\n' "$TERVO_PLUGIN_NAME"
printf 'api=%s\n' "$TERVO_PLUGIN_API"
if IFS= read -r line; then
  printf 'stdin=%s\n' "$line"
fi
printf 'err-line\n' >&2
exit 0
"#;
        write_script(&dir.join("run.sh"), script);
        let report = execute(
            "hello",
            &dir,
            &dir.join("run.sh"),
            "greet",
            &[],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(report.success(), "{report:?}");
        assert_eq!(report.exit_code, Some(0));
        assert!(!report.timed_out);
        assert!(report.stdout.contains("name=hello"), "{}", report.stdout);
        assert!(report.stdout.contains("api=1"), "{}", report.stdout);
        let json_line = report
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("stdin="))
            .expect("stdin 回显缺失");
        let payload: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert_eq!(payload["event"], "command");
        assert_eq!(payload["id"], "greet");
        assert_eq!(payload["args"], serde_json::json!([]));
        assert!(report.stderr.contains("err-line"), "{}", report.stderr);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn execute_reports_nonzero_exit_and_cwd() {
        let dir = tmpdir("exec-fail");
        write_script(
            &dir.join("run.sh"),
            "#!/bin/sh\npwd\nprintf 'bad\\n' >&2\nexit 7\n",
        );
        let report = execute(
            "hello",
            &dir,
            &dir.join("run.sh"),
            "greet",
            &[],
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!report.success());
        assert_eq!(report.exit_code, Some(7));
        assert!(!report.timed_out);
        assert_eq!(report.stdout.trim(), dir.canonicalize().unwrap().display().to_string());
        assert!(report.stderr.contains("bad"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn execute_times_out_and_kills() {
        let dir = tmpdir("exec-timeout");
        write_script(&dir.join("run.sh"), "#!/bin/sh\nexec sleep 30\n");
        let start = Instant::now();
        let report = execute(
            "hello",
            &dir,
            &dir.join("run.sh"),
            "slow",
            &[],
            Duration::from_millis(150),
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(report.timed_out, "{report:?}");
        assert!(!report.success());
        assert!(elapsed < Duration::from_secs(5), "超时 kill 太慢: {elapsed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn execute_reports_spawn_error_without_panic() {
        let dir = tmpdir("exec-missing");
        let err = execute(
            "hello",
            &dir,
            &dir.join("nope.sh"),
            "greet",
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(err.contains("无法启动"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_package_is_not_registered_or_executed() {
        let dir = tmpdir("disabled");
        let manifest = HELLO_MANIFEST.replace(
            "description = \"demo plugin\"",
            "description = \"demo plugin\"\nenabled = false",
        );
        write_package(&dir, "hello", &manifest, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 1);
        assert_eq!(store.enabled_count(), 0);
        assert_eq!(store.enabled_command_count(), 0);
        let mut registry = commands::builtin();
        let before = registry.len();
        assert_eq!(register_plugins(&mut registry, &store), 0);
        assert_eq!(registry.len(), before);
        assert!(registry.get("plugin.hello.greet").is_none());
        assert!(!store.spawn_command("plugin.hello.greet"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------- 事件钩子

    #[test]
    fn parses_event_subscriptions_and_rejects_unknown_or_duplicate() {
        for event in PluginEvent::ALL {
            assert_eq!(PluginEvent::parse(event.as_str()), Some(event));
        }
        assert_eq!(PluginEvent::parse(" start-up "), None);

        let dir = tmpdir("events-load");
        write_package(&dir, "events", EVENT_MANIFEST, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 1);
        let package = &store.packages[0];
        for event in PluginEvent::ALL {
            assert!(package.manifest.declares_event(event), "{event:?}");
        }
        assert!(package.display_line().contains("3 event(s)"), "{}", package.display_line());
        let _ = std::fs::remove_dir_all(&dir);

        let dir = tmpdir("events-unknown");
        let bad = EVENT_MANIFEST.replace("tab-close", "tab-explode");
        write_package(&dir, "events", &bad, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.packages.is_empty());
        assert_eq!(store.skipped.len(), 1);
        let reason = &store.skipped[0].1;
        assert!(reason.contains("events[3]"), "{reason}");
        assert!(reason.contains("tab-explode"), "{reason}");
        let _ = std::fs::remove_dir_all(&dir);

        let dir = tmpdir("events-dup");
        let duplicate = EVENT_MANIFEST.replace(
            "[\"startup\", \"tab-open\", \"tab-close\"]",
            "[\"startup\", \"startup\"]",
        );
        write_package(&dir, "events", &duplicate, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.packages.is_empty());
        assert!(store.skipped[0].1.contains("event 重复"), "{:?}", store.skipped);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_payloads_carry_only_title_and_kind() {
        assert_eq!(startup_payload(), serde_json::json!({}));

        let open = tab_open_payload("Build \u{0}\u{7}dev", "ssh");
        let object = open.as_object().expect("payload 必须是对象");
        assert_eq!(object.len(), 2, "{open}");
        assert_eq!(open["title"], "Build dev");
        assert_eq!(open["kind"], "ssh");
        // 异常 kind 回退 local，避免把任意字段带进插件 stdin
        assert_eq!(tab_open_payload("x", "ssh; rm -rf /")["kind"], "local");
        assert_eq!(tab_open_payload("x", "")["kind"], "local");

        let close = tab_close_payload("Terminal 2");
        assert_eq!(close, serde_json::json!({"title": "Terminal 2"}));
        assert_eq!(close.as_object().map(|o| o.len()), Some(1));

        // 标题超长按字符截断，不按字节
        let long_title = "中".repeat(MAX_EVENT_TITLE_CHARS + 20);
        let close = tab_close_payload(&long_title);
        let title = close["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), MAX_EVENT_TITLE_CHARS);
        assert!(title.ends_with('…'));

        let message = event_message(PluginEvent::TabOpen, &open);
        assert!(message.ends_with('\n'));
        assert_eq!(message.matches('\n').count(), 1, "协议必须是单行 JSON");
        let parsed: serde_json::Value = serde_json::from_str(message.trim_end()).unwrap();
        assert_eq!(parsed["event"], "tab-open");
        assert_eq!(parsed["payload"]["title"], "Build dev");
        assert_eq!(parsed["payload"]["kind"], "ssh");
        assert!(parsed.get("id").is_none());
        assert!(parsed["payload"].get("cwd").is_none());
    }

    #[test]
    fn event_targets_respect_declaration_and_enabled() {
        let dir = tmpdir("events-targets");
        let only_startup = EVENT_MANIFEST
            .replace("name = \"events\"", "name = \"onlystart\"")
            .replace(
                "events = [\"startup\", \"tab-open\", \"tab-close\"]",
                "events = [\"startup\"]",
            );
        write_package(&dir, "onlystart", &only_startup, Some("#!/bin/sh\nexit 0\n"));
        let off = EVENT_MANIFEST.replace(
            "entry = \"run.sh\"",
            "entry = \"run.sh\"\nenabled = false",
        );
        write_package(&dir, "off", &off, Some("#!/bin/sh\nexit 0\n"));
        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 2);
        assert_eq!(store.event_targets(PluginEvent::Startup).len(), 1);
        assert_eq!(store.event_targets(PluginEvent::TabOpen).len(), 0);
        assert_eq!(store.event_targets(PluginEvent::TabClose).len(), 0);
        // 没有插件声明 → 不 spawn（返回 0）
        assert_eq!(store.spawn_event(PluginEvent::TabOpen, tab_open_payload("x", "local")), 0);
        assert_eq!(store.spawn_event(PluginEvent::TabClose, tab_close_payload("x")), 0);
        // 只有启用的声明插件被提交；禁用插件不触发
        assert_eq!(store.spawn_event(PluginEvent::Startup, startup_payload()), 1);
        assert!(
            wait_until(|| !store.activity_snapshot().1.is_empty(), Duration::from_secs(3)),
            "事件活动未记录"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_event_sends_json_payload_to_declared_plugins_only() {
        let dir = tmpdir("events-spawn");
        write_package(&dir, "events", EVENT_MANIFEST, Some("#!/bin/sh\ncat >> events.log\n"));
        // 未声明任何 events 的插件：即使执行也不该被事件触发
        let silent = HELLO_MANIFEST.replace("name = \"hello\"", "name = \"silent\"");
        write_package(&dir, "silent", &silent, Some("#!/bin/sh\ncat >> silent.log\n"));
        // 声明了 events 但禁用：不触发
        let off = EVENT_MANIFEST
            .replace("name = \"events\"", "name = \"off\"")
            .replace("entry = \"run.sh\"", "entry = \"run.sh\"\nenabled = false");
        write_package(&dir, "off", &off, Some("#!/bin/sh\ncat >> off.log\n"));

        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 3);
        let log = dir.join("events").join("events.log");

        assert_eq!(store.spawn_event(PluginEvent::Startup, startup_payload()), 1);
        assert!(
            wait_until(|| read_lines(&log).len() == 1, Duration::from_secs(3)),
            "startup 事件未落盘"
        );
        let payload: serde_json::Value = serde_json::from_str(&read_lines(&log)[0]).unwrap();
        assert_eq!(payload["event"], "startup");
        assert_eq!(payload["payload"], serde_json::json!({}));

        assert_eq!(
            store.spawn_event(PluginEvent::TabOpen, tab_open_payload("Build: dev", "ssh")),
            1
        );
        assert!(
            wait_until(|| read_lines(&log).len() == 2, Duration::from_secs(3)),
            "tab-open 事件未落盘"
        );
        let payload: serde_json::Value = serde_json::from_str(&read_lines(&log)[1]).unwrap();
        assert_eq!(payload["event"], "tab-open");
        assert_eq!(payload["payload"]["title"], "Build: dev");
        assert_eq!(payload["payload"]["kind"], "ssh");

        assert_eq!(
            store.spawn_event(PluginEvent::TabClose, tab_close_payload("Build: dev")),
            1
        );
        assert!(
            wait_until(|| read_lines(&log).len() == 3, Duration::from_secs(3)),
            "tab-close 事件未落盘"
        );
        let payload: serde_json::Value = serde_json::from_str(&read_lines(&log)[2]).unwrap();
        assert_eq!(payload["event"], "tab-close");
        assert_eq!(payload["payload"], serde_json::json!({"title": "Build: dev"}));

        // 给可能错误 spawn 的线程一点时间，再确认它们没有输出
        std::thread::sleep(Duration::from_millis(150));
        assert!(!dir.join("silent").join("silent.log").exists());
        assert!(!dir.join("off").join("off.log").exists());

        let (_, lines) = store.activity_snapshot();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines.iter().all(|line| line.contains("event ") && line.contains("exit=0")), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn execute_event_sends_event_json_and_reports_failure() {
        let dir = tmpdir("exec-event");
        write_script(
            &dir.join("run.sh"),
            "#!/bin/sh\nIFS= read -r line\nprintf 'stdin=%s\\n' \"$line\"\nexit 3\n",
        );
        let report = execute_event(
            "events",
            &dir,
            &dir.join("run.sh"),
            PluginEvent::TabOpen,
            &tab_open_payload("Build: dev", "ssh"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(report.exit_code, Some(3));
        assert!(!report.success());
        let line = report
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("stdin="))
            .expect("stdin 回显缺失");
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["event"], "tab-open");
        assert_eq!(parsed["payload"]["title"], "Build: dev");
        assert_eq!(parsed["payload"]["kind"], "ssh");
        assert!(parsed["payload"].get("cwd").is_none());

        write_script(&dir.join("slow.sh"), "#!/bin/sh\nexec sleep 30\n");
        let report = execute_event(
            "events",
            &dir,
            &dir.join("slow.sh"),
            PluginEvent::Startup,
            &startup_payload(),
            Duration::from_millis(150),
        )
        .unwrap();
        assert!(report.timed_out, "{report:?}");
        let record = PluginActivity::from_report("events", "event startup", &report);
        assert_eq!(record.outcome, "timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn event_failure_updates_activity_without_blocking() {
        let dir = tmpdir("events-fail");
        write_package(
            &dir,
            "events",
            EVENT_MANIFEST,
            Some("#!/bin/sh\nprintf 'event-broke\\n' >&2\nexit 9\n"),
        );
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.spawn_event(PluginEvent::Startup, startup_payload()), 1);
        assert!(
            wait_until(|| !store.activity_snapshot().1.is_empty(), Duration::from_secs(3)),
            "失败事件未记录活动"
        );
        let line = &store.activity_snapshot().1[0];
        assert!(line.contains("event startup"), "{line}");
        assert!(line.contains("exit=9"), "{line}");
        assert!(line.contains("event-broke"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------- 最近活动

    #[test]
    fn activity_log_keeps_latest_and_sanitizes_summary() {
        let mut log = ActivityLog::default();
        assert!(log.is_empty());
        for i in 0..(ACTIVITY_CAPACITY + 3) {
            log.push(PluginActivity::new(
                format!("12:00:{i:02}"),
                "hello",
                "command greet",
                "exit=0",
                format!("line {i}"),
            ));
        }
        assert_eq!(log.len(), ACTIVITY_CAPACITY);
        assert_eq!(log.revision(), (ACTIVITY_CAPACITY + 3) as u64);
        let lines = log.lines();
        assert_eq!(lines.len(), ACTIVITY_CAPACITY);
        // 新 → 旧：最新一条在最前，最旧的两条被挤出
        assert!(lines[0].starts_with("12:00:10"), "{}", lines[0]);
        assert!(lines[0].contains("line 10"));
        assert!(lines.iter().all(|line| !line.ends_with("line 0") && !line.ends_with("line 1") && !line.ends_with("line 2")), "{lines:?}");
        assert!(lines[ACTIVITY_CAPACITY - 1].contains("line 3"), "{}", lines[ACTIVITY_CAPACITY - 1]);

        let base = ExecutionReport {
            exit_code: Some(0),
            timed_out: false,
            stdout: "第一行\u{0}\u{7}\nsecond\n".to_string(),
            stderr: "err-line".to_string(),
            stdout_truncated: false,
            stderr_truncated: false,
            elapsed: Duration::from_millis(12),
        };
        let record = PluginActivity::from_report("hello", "command greet", &base);
        assert_eq!(record.summary, "第一行");
        assert_eq!(record.outcome, "exit=0");
        let line = record.display_line();
        assert!(line.contains("hello · command greet · exit=0 · 第一行"), "{line}");
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(!line.contains('\u{0}') && !line.contains('\u{7}'));

        // 首个非空行超长 → 按字符截断并加省略号
        let long = ExecutionReport { stdout: "x".repeat(MAX_ACTIVITY_SUMMARY_CHARS * 3), ..base.clone() };
        let record = PluginActivity::from_report("hello", "command greet", &long);
        assert_eq!(record.summary.chars().count(), MAX_ACTIVITY_SUMMARY_CHARS);
        assert!(record.summary.ends_with('…'));

        // stdout 为空时回退 stderr 首行
        let stderr_only = ExecutionReport {
            stdout: " \n\t\n".to_string(),
            stderr: "boom\nmore".to_string(),
            ..base.clone()
        };
        assert_eq!(PluginActivity::from_report("hello", "command greet", &stderr_only).summary, "boom");

        // 超时 / 被信号杀死 / 进程没起来
        let timeout = ExecutionReport { timed_out: true, exit_code: None, ..base.clone() };
        assert_eq!(
            PluginActivity::from_report("hello", "event startup", &timeout).outcome,
            "timeout"
        );
        let signal = ExecutionReport { exit_code: None, ..base.clone() };
        assert_eq!(PluginActivity::from_report("hello", "event startup", &signal).outcome, "signal");
        let error = PluginActivity::from_error("hello", "event startup", "无法启动 x\u{0}");
        assert_eq!(error.outcome, "spawn-error");
        assert_eq!(error.summary, "无法启动 x");
        assert!(!error.display_line().contains('\u{0}'));
    }

    #[test]
    fn activity_push_wakes_ui_once_per_burst() {
        let (wake, rx) = WakeSignal::new();
        let mut log = ActivityLog::default();
        log.set_wake(wake);

        log.push(PluginActivity::new("12:00:00", "hello", "event startup", "exit=0", "one"));
        assert!(rx.try_recv().is_ok(), "push 后应立即唤醒 UI");
        assert_eq!(log.revision(), 1);

        // 连续 push 合并成一次唤醒；记录本身不合并。
        for i in 1..5 {
            log.push(PluginActivity::new(
                format!("12:00:0{i}"),
                "hello",
                "event startup",
                "exit=0",
                "buried",
            ));
        }
        assert!(rx.try_recv().is_ok(), "合并窗口内的第一次唤醒必须还在");
        assert!(rx.try_recv().is_err(), "同一轮唤醒必须合并成一次");
        assert_eq!(log.revision(), 5);
        assert_eq!(log.len(), 5);
        assert_eq!(log.lines().len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_command_records_activity_and_spawn_error() {
        let dir = tmpdir("activity-command");
        write_package(
            &dir,
            "hello",
            HELLO_MANIFEST,
            Some("#!/bin/sh\nprintf 'hi from plugin\\n'\nexit 0\n"),
        );
        let store = PluginStore::load_from(&dir);
        assert!(store.spawn_command("plugin.hello.greet"));
        assert!(
            wait_until(|| !store.activity_snapshot().1.is_empty(), Duration::from_secs(3)),
            "命令活动未记录"
        );
        let line = &store.activity_snapshot().1[0];
        assert!(line.contains("command greet"), "{line}");
        assert!(line.contains("exit=0"), "{line}");
        assert!(line.contains("hi from plugin"), "{line}");

        // 进程没起来（entry 丢失）：spawn_command 已经返回 true，错误只进活动/日志
        let mut broken = PluginStore::default();
        broken.packages.push(PluginPackage {
            manifest: PackageManifest {
                name: "broken".to_string(),
                version: "1".to_string(),
                api: PLUGIN_API_VERSION,
                entry: "nope.sh".to_string(),
                enabled: true,
                description: String::new(),
                commands: vec![PluginCommandDecl {
                    id: "boom".to_string(),
                    title: "Boom".to_string(),
                    key: None,
                }],
                events: Vec::new(),
            },
            dir: dir.clone(),
            entry: dir.join("nope.sh"),
        });
        assert!(broken.spawn_command("plugin.broken.boom"));
        assert!(
            wait_until(|| !broken.activity_snapshot().1.is_empty(), Duration::from_secs(3)),
            "spawn 错误未记录活动"
        );
        let line = &broken.activity_snapshot().1[0];
        assert!(line.contains("command boom"), "{line}");
        assert!(line.contains("spawn-error"), "{line}");
        assert!(line.contains("无法启动"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
