// SPDX-License-Identifier: GPL-3.0-only
//! 插件清单与外部进程宿主（V1）。
//!
//! 支持三种形态，互不替换：
//!
//! 1. **旧扁平清单**：`<plugins>/*.toml`（`id`/`name`/`version`/`description`/`enabled`）。
//!    仅发现、校验、展示，**不执行**；保留给旧配置与旧版设置页。
//! 2. **v1 oneshot 目录插件**：`<plugins>/<name>/plugin.toml`，无 `runtime` 或
//!    `runtime = "oneshot"`；命令/事件每次以后台线程 spawn `entry`，stdin 发一行 JSON，
//!    stdout/stderr 写日志，10s 超时 kill。兼容既有插件。
//! 3. **v1 process 目录插件**：`runtime = "process"`；每个插件一个 supervisor 线程，
//!    常驻子进程 + JSON Lines（init/ready/command/result/event/status/log/shutdown），
//!    2s 握手、10s 命令超时、崩溃重启上限 2、坏 JSON 禁用；命令结果回 UI 浮层/活动，
//!    `[contributes] status` 提供底部状态栏，`theme` 提供声明式主题。
//!
//! 安全边界：插件是**普通子进程**，拥有当前用户全部权限，没有沙箱/权限模型；
//! `[permissions]` 仅声明/展示，不授予能力。`entry` 只允许插件目录内的相对路径
//! （拒绝绝对路径、`..`、符号链接逃逸）；不 shell 拼接、不传 Vault 明文；
//! 事件 payload 只带 Tab 标题/类型，不含 cwd、命令行、环境变量或凭据。
//! 详见 `docs/PLUGIN_API.md`。

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
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

// ---------------------------------------------------------------- 常驻进程插件

/// 常驻进程插件启动并完成 `ready` 握手的上限；超时 kill 并按崩溃处理。
pub const PROCESS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// 常驻进程插件单条命令的默认超时；超时 kill 进程并按崩溃处理。
pub const PROCESS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// 状态提供者的轮询间隔（宿主主动请求，插件也可异步推送）。
pub const PROCESS_STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// 单次状态请求超时；超时只丢弃该次请求，不 kill 进程。
pub const PROCESS_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// 同一插件在本次 Tervo 运行内允许的自动重启次数；超过后禁用并显示原因。
pub const PROCESS_MAX_RESTARTS: u32 = 2;

/// 连续坏 JSON/超长行达到该数量后禁用插件，避免坏插件空转拖日志。
pub const PROCESS_MAX_BAD_LINES: u32 = 5;

/// 协议单行上限（字节）。超长行会被截断并计入坏行，不会无限分配内存。
pub const MAX_PROTOCOL_LINE_BYTES: usize = 64 * 1024;

/// 命令返回文本、状态文本与 tooltip 的最大字符数（按字符截断）。
pub const MAX_RESULT_TEXT_CHARS: usize = 2000;
pub const MAX_STATUS_TEXT_CHARS: usize = 200;
pub const MAX_TOOLTIP_CHARS: usize = 512;
/// 插件返回的 command 字段只允许短 ID。
const MAX_STATUS_COMMAND_CHARS: usize = 128;

/// 活动记录里进程生命周期摘要的最大字符数。
const MAX_LIFECYCLE_SUMMARY_CHARS: usize = 140;

/// 最近未展示的插件通知（命令结果）上限；UI 每轮取走。
const NOTICE_CAPACITY: usize = 16;

/// 预留：插件协议版本（与 api 一起写在 init 行里）。
const PLUGIN_PROTOCOL_VERSION: u32 = 1;

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
    /// 可选静态参数（V1：命令面板不弹参数输入，按清单原样下发到协议 `args`）。
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

/// 插件运行形态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginRuntimeKind {
    /// 旧行为：每次命令/事件启动一个新进程，发一行 JSON 后退出。
    Oneshot,
    /// V1 常驻进程：启动时握手，JSON Lines 双向通信，崩溃可重启（有上限）。
    Process,
}

impl PluginRuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oneshot => "oneshot",
            Self::Process => "process",
        }
    }
}

/// 权限声明（**仅声明 + 展示，当前不授予、不沙箱**）。
///
/// 所有字段默认拒绝；宿主 V1 仍以普通子进程运行插件，拥有当前用户权限。
/// 声明用于让设置页/文档明确“插件想要什么”，真正执行仍靠用户信任。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginPermissions {
    /// `none` | `read-plugin` | `read-home` | `custom`（仅声明，不实现）。
    pub filesystem: String,
    pub network: bool,
    pub process: bool,
    pub clipboard: bool,
    /// `none` | `profile-ref`（仅声明；V1 不把任何 Vault 明文传给插件）。
    pub credentials: String,
}

impl Default for PluginPermissions {
    fn default() -> Self {
        Self {
            filesystem: "none".to_string(),
            network: false,
            process: false,
            clipboard: false,
            credentials: "none".to_string(),
        }
    }
}

impl PluginPermissions {
    pub fn validate(&self) -> Result<(), String> {
        let fs = self.filesystem.trim();
        if !matches!(fs, "none" | "read-plugin" | "read-home" | "custom") {
            return Err(format!(
                "permissions.filesystem 只支持 none/read-plugin/read-home/custom（当前 {fs:?}）"
            ));
        }
        let cred = self.credentials.trim();
        if !matches!(cred, "none" | "profile-ref") {
            return Err(format!(
                "permissions.credentials 只支持 none/profile-ref（当前 {cred:?}）"
            ));
        }
        Ok(())
    }

    pub fn declares_anything(&self) -> bool {
        self.filesystem != "none"
            || self.network
            || self.process
            || self.clipboard
            || self.credentials != "none"
    }

    /// 设置页/日志用的一行；未声明任何权限时返回空串。
    pub fn summary(&self) -> String {
        if !self.declares_anything() {
            return String::new();
        }
        format!(
            "fs={} net={} process={} clipboard={} credentials={}（仅声明/未授予）",
            self.filesystem, self.network, self.process, self.clipboard, self.credentials
        )
    }
}

/// 插件声明的贡献点。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginContributes {
    /// 可选交叉校验：列出的命令必须存在于 `[[commands]]`。
    pub commands: Vec<String>,
    /// 是否提供状态（周期轮询 + 可异步推送）。
    pub status: bool,
    /// 可选声明式 TOML 主题（插件目录内相对路径）。
    pub theme: Option<String>,
}

impl PluginContributes {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(theme) = &self.theme {
            entry_path(theme).map_err(|e| format!("contributes.theme 非法: {e}"))?;
        }
        Ok(())
    }
}

/// v1 目录插件清单：`<plugins>/<name>/plugin.toml`。
///
/// 兼容旧的扁平字段（`name/version/api/entry/enabled/description/commands/events`），
/// 新增可选 `id`、`runtime`、`[permissions]`、`[contributes]`。旧清单缺省
/// `runtime = "oneshot"`，行为与之前完全一致。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackageManifest {
    /// 稳定 ID（默认 = name）；允许反向域名点号，用于展示/去重。
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub version: String,
    /// 必须等于 [`PLUGIN_API_VERSION`]。
    pub api: u32,
    /// `process`（常驻、握手/JSON Lines）或 `oneshot`（旧行为）；缺省 oneshot。
    #[serde(default)]
    pub runtime: Option<String>,
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
    #[serde(default)]
    pub permissions: PluginPermissions,
    #[serde(default)]
    pub contributes: PluginContributes,
}

impl PackageManifest {
    /// 稳定 ID：显式 `id` 优先，否则回退到 `name`。
    pub fn plugin_id(&self) -> &str {
        if self.id.trim().is_empty() {
            &self.name
        } else {
            self.id.trim()
        }
    }

    pub fn runtime_kind(&self) -> Result<PluginRuntimeKind, String> {
        match self.runtime.as_deref().map(str::trim) {
            None | Some("") | Some("oneshot") => Ok(PluginRuntimeKind::Oneshot),
            Some("process") => Ok(PluginRuntimeKind::Process),
            Some(other) => Err(format!(
                "不支持的 runtime {other:?}（只支持 process / oneshot；缺省 oneshot）"
            )),
        }
    }

    /// 不访问文件系统的字段校验；`entry` 是否存在/是否逃逸在加载时另行检查。
    pub fn validate(&self) -> Result<(), String> {
        validate_plugin_name(&self.name)?;
        if !self.id.trim().is_empty() {
            validate_plugin_id(self.plugin_id())?;
        }
        if self.version.trim().is_empty() {
            return Err(format!("插件 {} 缺少 version", self.name));
        }
        if self.api != PLUGIN_API_VERSION {
            return Err(format!(
                "不支持的 api {}（宿主只支持 api={PLUGIN_API_VERSION}）",
                self.api
            ));
        }
        self.runtime_kind()?;
        entry_path(&self.entry)?;
        self.permissions.validate()?;
        self.contributes.validate()?;
        if self.contributes.status && self.runtime_kind()? != PluginRuntimeKind::Process {
            return Err(format!(
                "插件 {} 声明了 contributes.status，但 runtime 不是 process",
                self.name
            ));
        }
        for (i, command) in self.commands.iter().enumerate() {
            validate_command_id(&command.id)
                .map_err(|e| format!("插件 {} 的命令 #{} 非法: {e}", self.name, i + 1))?;
            if command.title.trim().is_empty() {
                return Err(format!("插件 {} 的命令 {} 缺少 title", self.name, command.id));
            }
            if command.args.len() > 16 {
                return Err(format!("插件 {} 命令 {} 的 args 最多 16 个", self.name, command.id));
            }
            for (j, arg) in command.args.iter().enumerate() {
                if arg.chars().count() > 256 {
                    return Err(format!(
                        "插件 {} 命令 {} 的 args[{}] 超过 256 字符",
                        self.name,
                        command.id,
                        j + 1
                    ));
                }
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
        for (i, declared) in self.contributes.commands.iter().enumerate() {
            if !self.commands.iter().any(|c| c.id == *declared) {
                return Err(format!(
                    "插件 {} 的 contributes.commands[{}] 未在 [[commands]] 中声明: {declared:?}",
                    self.name,
                    i + 1
                ));
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

    pub fn is_process(&self) -> bool {
        self.runtime_kind() == Ok(PluginRuntimeKind::Process)
    }
}

fn validate_plugin_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err(format!("插件 id 必须非空且不超过 128 字节: {id:?}"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!("插件 id 只能包含字母/数字/-/_/. : {id}"));
    }
    Ok(())
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

/// 解析并校验主题文件的真实路径：必须存在、是普通文件、canonical 后仍在插件目录内。
fn resolve_theme_file(plugin_dir: &Path, theme: &str) -> Result<PathBuf, String> {
    let relative = entry_path(theme)?;
    let canonical_dir = std::fs::canonicalize(plugin_dir)
        .map_err(|e| format!("插件目录不可访问 {}: {e}", plugin_dir.display()))?;
    let joined = canonical_dir.join(relative);
    let canonical_file = std::fs::canonicalize(&joined)
        .map_err(|e| format!("主题文件不存在或不可访问 {}: {e}", joined.display()))?;
    if !canonical_file.starts_with(&canonical_dir) {
        return Err(format!("主题文件解析后逃出插件目录: {}", canonical_file.display()));
    }
    if !canonical_file.is_file() {
        return Err(format!("主题不是普通文件: {}", canonical_file.display()));
    }
    Ok(canonical_file)
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

/// 常驻进程插件的运行状态（设置页展示；崩溃后自动重启，超限禁用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimePhase {
    Starting,
    Running,
    Restarting,
    Disabled,
    Stopped,
}

impl RuntimePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Disabled => "disabled",
            Self::Stopped => "stopped",
        }
    }
}

/// 常驻进程插件状态快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub phase: RuntimePhase,
    /// 禁用/重启原因（空 = 正常）。
    pub error: String,
    pub restarts: u32,
    pub pid: Option<u32>,
}

impl Default for RuntimeInfo {
    fn default() -> Self {
        Self {
            phase: RuntimePhase::Stopped,
            error: String::new(),
            restarts: 0,
            pid: None,
        }
    }
}

impl RuntimeInfo {
    #[allow(dead_code)] // 单测/后续状态页使用；生产 UI 只读 display()
    pub fn is_healthy(&self) -> bool {
        self.phase == RuntimePhase::Running
    }

    /// 设置页展示：`running` / `disabled: <原因>` / `restarting (2/2)`。
    pub fn display(&self) -> String {
        match self.phase {
            RuntimePhase::Running | RuntimePhase::Starting | RuntimePhase::Stopped => {
                self.phase.as_str().to_string()
            },
            RuntimePhase::Restarting => {
                format!("restarting {}/{}", self.restarts, PROCESS_MAX_RESTARTS)
            },
            RuntimePhase::Disabled => {
                let reason = sanitize_activity_text(&self.error);
                if reason.is_empty() {
                    "disabled".to_string()
                } else {
                    format!("disabled: {reason}")
                }
            },
        }
    }
}

/// 一个已加载的 v1 目录插件。
#[derive(Clone, Debug)]
pub struct PluginPackage {
    pub manifest: PackageManifest,
    /// 插件目录（canonical；执行时作为 cwd）。
    pub dir: PathBuf,
    /// 已解析的 entry 绝对路径（canonical）。
    pub entry: PathBuf,
    /// 可选声明式主题文件的 canonical 路径（`[contributes] theme`）。
    pub theme_path: Option<PathBuf>,
    /// 主题声明存在但文件/路径不合法时的原因（插件仍加载，主题跳过）。
    pub theme_error: Option<String>,
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
    #[allow(dead_code)] // 测试/诊断用；生产走 display_line_with(runtime)
    pub fn display_line(&self) -> String {
        self.display_line_with(None)
    }

    /// 带运行状态的一行（常驻进程插件传 runtime；oneshot 传 None）。
    pub fn display_line_with(&self, runtime: Option<&RuntimeInfo>) -> String {
        let state = if self.manifest.enabled { "enabled" } else { "disabled" };
        let mut line = format!(
            "{} v{} — {state}",
            self.manifest.name, self.manifest.version
        );
        // 反向域名 id 与 name 不同时展示，便于对应插件目录。
        if self.manifest.plugin_id() != self.manifest.name {
            line.push_str(&format!(" · id={}", self.manifest.plugin_id()));
        }
        line.push_str(&format!(" · api {}", self.manifest.api));
        line.push_str(&format!(" · {}", self.manifest.runtime_kind().map(|k| k.as_str()).unwrap_or("invalid")));
        line.push_str(&format!(" · {} command(s)", self.command_count()));
        if !self.manifest.events.is_empty() {
            line.push_str(&format!(" · {} event(s)", self.manifest.events.len()));
        }
        if self.manifest.contributes.status {
            line.push_str(" · status");
        }
        if let Some(theme) = &self.manifest.contributes.theme {
            let path = theme.trim();
            if !path.is_empty() {
                line.push_str(" · theme");
            }
        }
        if let Some(runtime) = runtime {
            line.push_str(" · ");
            line.push_str(&runtime.display());
        }
        if let Some(error) = &self.theme_error {
            line.push_str(" · theme-error: ");
            line.push_str(&sanitize_activity_text(error));
        }
        let perms = self.manifest.permissions.summary();
        if !perms.is_empty() {
            line.push_str(" · ");
            line.push_str(&perms);
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
    /// 常驻进程插件的 supervisor 句柄（`start_process_plugins` 后填充）。
    processes: Vec<ProcessPluginHandle>,
    process_started: bool,
    /// 状态文本/进程状态的 revision，UI 只在变化时刷 Slint model。
    status_revision: Arc<AtomicU64>,
    runtime_revision: Arc<AtomicU64>,
    /// 后台命令结果，UI 下一轮取走并显示浮层。
    notices: Arc<Mutex<NoticeQueue>>,
    /// 插件活动/状态变化唤醒 UI（与 ActivityLog 共用同一信号）。
    wake: Arc<Mutex<Option<Arc<WakeSignal>>>>,
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
    wake: Option<Arc<WakeSignal>>,
}

impl ActivityLog {
    /// 安装唤醒信号；`PluginStore` 在启动时注入，测试可用默认空值。
    #[allow(dead_code)] // 单测直接持有 ActivityLog 时使用；生产走 set_wake_arc
    pub fn set_wake(&mut self, wake: WakeSignal) {
        self.wake = Some(Arc::new(wake));
    }

    /// 与常驻进程 supervisor 共享同一个唤醒信号。
    fn set_wake_arc(&mut self, wake: Arc<WakeSignal>) {
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

// ---------------------------------------------------------------- 常驻进程运行时

/// 状态提供者返回的一条状态（设置页/底部最小状态栏共用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginStatusItem {
    pub plugin: String,
    pub text: String,
    pub tooltip: String,
    /// 点击状态栏时执行的命令 ID（空 = 不可点击）。
    pub command: String,
}

/// 命令结果通知：UI 下一轮取走并显示在底部浮层（同时写活动/日志）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginNotice {
    pub plugin: String,
    pub text: String,
    pub ok: bool,
}

#[derive(Clone, Debug, Default)]
struct NoticeQueue {
    records: VecDeque<PluginNotice>,
}

impl NoticeQueue {
    fn push(&mut self, notice: PluginNotice) {
        if self.records.len() >= NOTICE_CAPACITY {
            self.records.pop_front();
        }
        self.records.push_back(notice);
    }

    fn drain(&mut self) -> Vec<PluginNotice> {
        self.records.drain(..).collect()
    }
}

fn push_notice(notices: &Arc<Mutex<NoticeQueue>>, notice: PluginNotice) {
    match notices.lock() {
        Ok(mut queue) => queue.push(notice),
        Err(_) => eprintln!("[plugin] 通知队列锁被污染，丢弃一条通知"),
    }
}

/// 常驻进程命令的执行结果（由 supervisor 回传给等待线程）。
#[derive(Clone, Debug, PartialEq, Eq)]
enum CommandOutcome {
    Ok(String),
    Error(String),
    Timeout,
    Unavailable(String),
}

impl CommandOutcome {
    fn ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    fn outcome_label(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::Error(_) => "error",
            Self::Timeout => "timeout",
            Self::Unavailable(_) => "unavailable",
        }
    }

    fn text(&self) -> &str {
        match self {
            Self::Ok(text) | Self::Error(text) | Self::Unavailable(text) => text,
            Self::Timeout => "命令超时（已 kill 插件进程）",
        }
    }
}

/// supervisor 接收的宿主请求。
enum SupervisorMsg {
    Execute {
        command: String,
        args: Vec<String>,
        reply: Sender<CommandOutcome>,
    },
    Event {
        event: PluginEvent,
        payload: serde_json::Value,
    },
    Shutdown,
}

/// stdout 读取线程解析后的消息。
enum HostIncoming {
    Ready(u32),
    Result { id: u64, ok: bool, text: String },
    Status { text: String, tooltip: String, command: String },
    Log(String),
    BadLine(String),
    Eof,
}

/// 一个常驻插件进程的句柄。
#[derive(Clone, Debug)]
struct ProcessPluginHandle {
    name: String,
    tx: Sender<SupervisorMsg>,
    info: Arc<Mutex<RuntimeInfo>>,
    status: Arc<Mutex<PluginStatusItem>>,
    has_status: bool,
    shutdown: Arc<AtomicBool>,
}

impl ProcessPluginHandle {
    fn info(&self) -> RuntimeInfo {
        self.info.lock().map(|i| i.clone()).unwrap_or_default()
    }
}

struct PendingRequest {
    kind: PendingKind,
    deadline: Instant,
    label: String,
    reply: Option<Sender<CommandOutcome>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Command,
    Status,
}

enum InstanceEnd {
    Shutdown,
    /// 可以按崩溃处理并重启（握手失败/进程退出/命令超时）。
    Crashed(String),
    /// 不可恢复（entry 丢失、坏 JSON 超限）：直接禁用。
    Disabled(String),
}

fn sanitize_short(text: &str, max: usize) -> String {
    crate::activity::sanitize_notification_text(text, max)
}

fn set_runtime(
    info: &Arc<Mutex<RuntimeInfo>>,
    revision: &Arc<AtomicU64>,
    phase: RuntimePhase,
    error: String,
    restarts: u32,
    pid: Option<u32>,
) {
    if let Ok(mut guard) = info.lock() {
        guard.phase = phase;
        guard.error = error;
        guard.restarts = restarts;
        guard.pid = pid;
    }
    revision.fetch_add(1, Ordering::Relaxed);
}

fn notify_wake(wake: &Arc<Mutex<Option<Arc<WakeSignal>>>>) {
    if let Ok(guard) = wake.lock() {
        if let Some(signal) = guard.as_ref() {
            signal.notify();
        }
    }
}

fn lifecycle_activity(
    activity: &Arc<Mutex<ActivityLog>>,
    plugin: &str,
    outcome: &str,
    summary: &str,
) {
    record_activity(
        activity,
        PluginActivity::new(
            now_hms(),
            plugin,
            "lifecycle",
            outcome,
            sanitize_short(summary, MAX_LIFECYCLE_SUMMARY_CHARS),
        ),
    );
}

/// 启动所有已启用 `runtime = "process"` 插件。非阻塞：每个插件一个 supervisor 线程，
/// 进程启动/握手/重启都在后台完成；坏插件不会阻塞 UI 或其它插件。
impl PluginStore {
    pub fn start_process_plugins(&mut self) {
        self.start_process_plugins_with_timeout(PROCESS_COMMAND_TIMEOUT);
    }

    /// 测试钩子：用更短命令超时启动，避免单测等 10s；生产只用默认值。
    pub(crate) fn start_process_plugins_with_timeout(&mut self, command_timeout: Duration) {
        if self.process_started {
            return;
        }
        self.process_started = true;
        let mut started = 0;
        for package in &self.packages {
            if !package.manifest.enabled || !package.manifest.is_process() {
                continue;
            }
            let handle = spawn_process_supervisor(
                package.clone(),
                self.activity.clone(),
                self.status_revision.clone(),
                self.runtime_revision.clone(),
                self.wake.clone(),
                command_timeout,
            );
            self.processes.push(handle);
            started += 1;
        }
        if started > 0 {
            eprintln!(
                "[plugin] 已启动 {} 个常驻进程插件（握手超时 {}ms，命令超时 {}ms，重启上限 {PROCESS_MAX_RESTARTS}）",
                started,
                PROCESS_HANDSHAKE_TIMEOUT.as_millis(),
                command_timeout.as_millis()
            );
        }
    }

    /// 关闭所有常驻插件进程并等待 supervisor 退出（窗口关闭/测试清理）。
    pub fn shutdown_processes(&self) {
        for process in &self.processes {
            process.shutdown.store(true, Ordering::Relaxed);
            let _ = process.tx.send(SupervisorMsg::Shutdown);
        }
        let deadline = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < deadline {
            let busy = self.processes.iter().any(|p| {
                matches!(
                    p.info().phase,
                    RuntimePhase::Starting | RuntimePhase::Running | RuntimePhase::Restarting
                )
            });
            if !busy {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn runtime_info(&self, name: &str) -> Option<RuntimeInfo> {
        self.processes
            .iter()
            .find(|p| p.name == name)
            .map(ProcessPluginHandle::info)
    }

    pub fn runtime_revision(&self) -> u64 {
        self.runtime_revision.load(Ordering::Relaxed)
    }

    /// 汇总所有常驻插件的状态文本；UI 只在 revision 变化时刷新。
    pub fn status_snapshot(&self) -> (u64, Vec<PluginStatusItem>) {
        let revision = self.status_revision.load(Ordering::Relaxed);
        let items = self
            .processes
            .iter()
            .filter(|p| p.has_status)
            .filter_map(|p| {
                let status = p.status.lock().ok()?.clone();
                if status.text.is_empty() {
                    None
                } else {
                    Some(status)
                }
            })
            .collect();
        (revision, items)
    }

    /// UI 取走命令结果通知（最多 [`NOTICE_CAPACITY`] 条，新→旧顺序由队列保证）。
    pub fn take_notices(&self) -> Vec<PluginNotice> {
        match self.notices.lock() {
            Ok(mut queue) => queue.drain(),
            Err(_) => Vec::new(),
        }
    }

    fn process_handle(&self, name: &str) -> Option<&ProcessPluginHandle> {
        self.processes.iter().find(|p| p.name == name)
    }

    fn spawn_process_command(&self, package: &PluginPackage, decl: &PluginCommandDecl) -> bool {
        let plugin_name = package.manifest.name.clone();
        let action = format!("command {}", decl.id);
        let full_id = package.full_command_id(&decl.id);
        let Some(handle) = self.process_handle(&plugin_name) else {
            let reason = "插件进程未启动（runtime=process 需宿主启动事件循环）".to_string();
            record_activity(
                &self.activity,
                PluginActivity::new(now_hms(), &plugin_name, &action, "unavailable", &reason),
            );
            push_notice(
                &self.notices,
                PluginNotice { plugin: plugin_name, text: reason, ok: false },
            );
            notify_wake(&self.wake);
            return true;
        };
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        if handle
            .tx
            .send(SupervisorMsg::Execute {
                command: decl.id.clone(),
                args: decl.args.clone(),
                reply: reply_tx,
            })
            .is_err()
        {
            let reason = "插件 supervisor 已退出".to_string();
            record_activity(
                &self.activity,
                PluginActivity::new(now_hms(), &plugin_name, &action, "unavailable", &reason),
            );
            push_notice(
                &self.notices,
                PluginNotice { plugin: plugin_name, text: reason, ok: false },
            );
            notify_wake(&self.wake);
            return true;
        }
        let activity = self.activity.clone();
        let notices = self.notices.clone();
        let wake = self.wake.clone();
        let plugin_for_thread = plugin_name.clone();
        let action_for_thread = action.clone();
        let label = full_id.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("plugin-cmd-{plugin_name}"))
            .spawn(move || {
                let outcome = reply_rx
                    .recv_timeout(PROCESS_COMMAND_TIMEOUT + Duration::from_secs(2))
                    .unwrap_or_else(|_| CommandOutcome::Unavailable("等待插件响应超时".to_string()));
                let text = sanitize_short(outcome.text(), MAX_RESULT_TEXT_CHARS);
                record_activity(
                    &activity,
                    PluginActivity::new(
                        now_hms(),
                        &plugin_for_thread,
                        &action_for_thread,
                        outcome.outcome_label(),
                        sanitize_short(&text, MAX_ACTIVITY_SUMMARY_CHARS),
                    ),
                );
                eprintln!(
                    "[plugin] {label} {}: {text}",
                    outcome.outcome_label()
                );
                push_notice(
                    &notices,
                    PluginNotice {
                        plugin: plugin_for_thread,
                        text: sanitize_short(&text, 300),
                        ok: outcome.ok(),
                    },
                );
                notify_wake(&wake);
            });
        match spawned {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[plugin] {full_id}: 无法创建命令等待线程: {e}");
                record_activity(
                    &self.activity,
                    PluginActivity::new(
                        now_hms(),
                        &plugin_name,
                        &action,
                        "spawn-error",
                        sanitize_short(&format!("无法创建等待线程: {e}"), MAX_ACTIVITY_SUMMARY_CHARS),
                    ),
                );
                true
            },
        }
    }
}

impl Drop for PluginStore {
    fn drop(&mut self) {
        self.shutdown_processes();
    }
}

/// 创建 supervisor 线程并返回句柄；线程内负责启动/握手/重启/禁用。
fn spawn_process_supervisor(
    package: PluginPackage,
    activity: Arc<Mutex<ActivityLog>>,
    status_revision: Arc<AtomicU64>,
    runtime_revision: Arc<AtomicU64>,
    wake: Arc<Mutex<Option<Arc<WakeSignal>>>>,
    command_timeout: Duration,
) -> ProcessPluginHandle {
    let name = package.manifest.name.clone();
    let (tx, rx) = crossbeam_channel::unbounded();
    let info = Arc::new(Mutex::new(RuntimeInfo {
        phase: RuntimePhase::Starting,
        ..RuntimeInfo::default()
    }));
    let status = Arc::new(Mutex::new(PluginStatusItem {
        plugin: name.clone(),
        text: String::new(),
        tooltip: String::new(),
        command: String::new(),
    }));
    let has_status = package.manifest.contributes.status;
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = ProcessPluginHandle {
        name: name.clone(),
        tx,
        info: info.clone(),
        status: status.clone(),
        has_status,
        shutdown: shutdown.clone(),
    };

    let thread_name = format!("plugin-host-{name}");
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
        supervisor_loop(
            package,
            rx,
            info,
            status,
            status_revision,
            runtime_revision,
            activity,
            wake,
            shutdown,
            command_timeout,
        );
    });
    if let Err(e) = spawn_result {
        eprintln!("[plugin] {name}: 无法创建 supervisor 线程: {e}");
        if let Ok(mut guard) = handle.info.lock() {
            guard.phase = RuntimePhase::Disabled;
            guard.error = format!("无法创建 supervisor 线程: {e}");
        }
    }
    handle
}

#[allow(clippy::too_many_arguments)]
fn supervisor_loop(
    package: PluginPackage,
    rx: Receiver<SupervisorMsg>,
    info: Arc<Mutex<RuntimeInfo>>,
    status: Arc<Mutex<PluginStatusItem>>,
    status_revision: Arc<AtomicU64>,
    runtime_revision: Arc<AtomicU64>,
    activity: Arc<Mutex<ActivityLog>>,
    wake: Arc<Mutex<Option<Arc<WakeSignal>>>>,
    shutdown: Arc<AtomicBool>,
    command_timeout: Duration,
) {
    let plugin = package.manifest.name.clone();
    let has_status = package.manifest.contributes.status;
    let mut restarts = 0u32;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            set_runtime(&info, &runtime_revision, RuntimePhase::Stopped, String::new(), restarts, None);
            break;
        }
        let phase = if restarts == 0 {
            RuntimePhase::Starting
        } else {
            RuntimePhase::Restarting
        };
        set_runtime(&info, &runtime_revision, phase, String::new(), restarts, None);
        match run_instance(
            &package,
            &rx,
            &info,
            &status,
            &status_revision,
            &runtime_revision,
            &wake,
            &shutdown,
            has_status,
            restarts,
            command_timeout,
        ) {
            InstanceEnd::Shutdown => {
                clear_status(&status, &status_revision, &wake);
                set_runtime(
                    &info,
                    &runtime_revision,
                    RuntimePhase::Stopped,
                    String::new(),
                    restarts,
                    None,
                );
                break;
            },
            InstanceEnd::Disabled(reason) => {
                clear_status(&status, &status_revision, &wake);
                eprintln!("[plugin] {plugin}: 已禁用：{reason}");
                lifecycle_activity(&activity, &plugin, "disabled", &reason);
                set_runtime(
                    &info,
                    &runtime_revision,
                    RuntimePhase::Disabled,
                    reason,
                    restarts,
                    None,
                );
                break;
            },
            InstanceEnd::Crashed(reason) => {
                clear_status(&status, &status_revision, &wake);
                restarts += 1;
                lifecycle_activity(
                    &activity,
                    &plugin,
                    &format!("crash {}", restarts),
                    &reason,
                );
                if restarts > PROCESS_MAX_RESTARTS {
                    let reason = format!(
                        "达到重启上限（{} 次）：{reason}",
                        PROCESS_MAX_RESTARTS
                    );
                    eprintln!("[plugin] {plugin}: 已禁用：{reason}");
                    lifecycle_activity(&activity, &plugin, "disabled", &reason);
                    set_runtime(
                        &info,
                        &runtime_revision,
                        RuntimePhase::Disabled,
                        reason,
                        restarts - 1,
                        None,
                    );
                    break;
                }
                eprintln!(
                    "[plugin] {plugin}: 进程异常（{}），准备重启 {}/{}",
                    sanitize_short(&reason, MAX_LIFECYCLE_SUMMARY_CHARS),
                    restarts,
                    PROCESS_MAX_RESTARTS
                );
                set_runtime(
                    &info,
                    &runtime_revision,
                    RuntimePhase::Restarting,
                    reason,
                    restarts,
                    None,
                );
                std::thread::sleep(Duration::from_millis(300));
            },
        }
    }
}

fn clear_status(
    status: &Arc<Mutex<PluginStatusItem>>,
    revision: &Arc<AtomicU64>,
    wake: &Arc<Mutex<Option<Arc<WakeSignal>>>>,
) {
    let changed = match status.lock() {
        Ok(mut status) => {
            if status.text.is_empty() && status.tooltip.is_empty() && status.command.is_empty() {
                false
            } else {
                status.text.clear();
                status.tooltip.clear();
                status.command.clear();
                true
            }
        },
        Err(_) => false,
    };
    if changed {
        revision.fetch_add(1, Ordering::Relaxed);
        notify_wake(wake);
    }
}

/// 启动并服务一次插件进程生命周期；退出时返回下一次动作。
#[allow(clippy::too_many_arguments)]
fn run_instance(
    package: &PluginPackage,
    rx: &Receiver<SupervisorMsg>,
    info: &Arc<Mutex<RuntimeInfo>>,
    status: &Arc<Mutex<PluginStatusItem>>,
    status_revision: &Arc<AtomicU64>,
    runtime_revision: &Arc<AtomicU64>,
    wake: &Arc<Mutex<Option<Arc<WakeSignal>>>>,
    shutdown: &Arc<AtomicBool>,
    has_status: bool,
    restarts: u32,
    command_timeout: Duration,
) -> InstanceEnd {
    let plugin = package.manifest.name.clone();
    let SpawnedChild {
        child,
        stdin,
        incoming_rx,
        stdout_thread,
        stderr_thread,
    } = match spawn_plugin_child(package) {
        Ok(spawned) => spawned,
        Err(e) => {
            eprintln!("[plugin] {plugin}: 无法启动常驻进程：{e}");
            return InstanceEnd::Disabled(format!("无法启动: {e}"));
        },
    };
    // reader 线程句柄 detach：进程被 kill 后管道关闭，线程自行退出；不 join 以免
    // 坏插件在退出路径上卡住宿主。
    drop(stdout_thread);
    drop(stderr_thread);
    let mut child = child.expect("child");
    let mut stdin = stdin.expect("stdin");
    let incoming_rx = incoming_rx.expect("incoming_rx");

    let pid = child.id();
    if let Ok(mut guard) = info.lock() {
        guard.pid = Some(pid);
    }
    runtime_revision.fetch_add(1, Ordering::Relaxed);

    // init：插件必须在 2s 内回 ready；否则 kill 并重试（有上限）。
    let init = serde_json::json!({
        "type": "init",
        "api": PLUGIN_API_VERSION,
        "protocol": PLUGIN_PROTOCOL_VERSION,
        "plugin": plugin,
        "host": "tervo",
    });
    if let Err(e) = send_json_line(&mut stdin, &init) {
        kill_child(&mut child);
        return InstanceEnd::Crashed(format!("写入 init 失败: {e}"));
    }

    let handshake_deadline = Instant::now() + PROCESS_HANDSHAKE_TIMEOUT;
    let mut ready = false;
    let mut bad_lines = 0u32;
    while Instant::now() < handshake_deadline {
        if shutdown.load(Ordering::Relaxed) {
            shutdown_child(&mut child, &mut stdin);
            return InstanceEnd::Shutdown;
        }
        if let Ok(Some(_status)) = child.try_wait() {
            return InstanceEnd::Crashed("握手前进程退出".to_string());
        }
        match incoming_rx.try_recv() {
            Ok(HostIncoming::Ready(api)) => {
                if api != PLUGIN_API_VERSION {
                    kill_child(&mut child);
                    return InstanceEnd::Disabled(format!(
                        "握手 api={api} 与宿主 api={PLUGIN_API_VERSION} 不兼容"
                    ));
                }
                ready = true;
                break;
            },
            Ok(HostIncoming::BadLine(line)) => {
                bad_lines += 1;
                eprintln!(
                    "[plugin] {plugin}: 握手期坏 JSON（{}/{}）: {}",
                    bad_lines,
                    PROCESS_MAX_BAD_LINES,
                    sanitize_short(&line, 160)
                );
                if bad_lines >= PROCESS_MAX_BAD_LINES {
                    kill_child(&mut child);
                    return InstanceEnd::Disabled("坏 JSON 过多".to_string());
                }
            },
            Ok(HostIncoming::Eof) => {
                kill_child(&mut child);
                return InstanceEnd::Crashed("握手前 stdout 关闭".to_string());
            },
            Ok(_) => {}, // 握手期其它消息先忽略
            Err(crossbeam_channel::TryRecvError::Empty) => {},
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                kill_child(&mut child);
                return InstanceEnd::Crashed("stdout reader 退出".to_string());
            },
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !ready {
        kill_child(&mut child);
        return InstanceEnd::Crashed(format!(
            "握手超时（{}ms 内未 ready）",
            PROCESS_HANDSHAKE_TIMEOUT.as_millis()
        ));
    }

    set_runtime(
        info,
        runtime_revision,
        RuntimePhase::Running,
        String::new(),
        restarts,
        Some(child.id()),
    );
    eprintln!("[plugin] {plugin}: 常驻进程就绪 pid={}", child.id());

    let mut pending: HashMap<u64, PendingRequest> = HashMap::new();
    let mut next_id = 1u64;
    let mut status_due = Instant::now() + Duration::from_millis(200);
    let mut status_errors = 0u32;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            fail_pending(&mut pending, CommandOutcome::Unavailable("宿主退出".to_string()));
            shutdown_child(&mut child, &mut stdin);
            return InstanceEnd::Shutdown;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                fail_pending(
                    &mut pending,
                    CommandOutcome::Unavailable(format!("插件进程退出（{:?}）", exit.code())),
                );
                return InstanceEnd::Crashed(format!("进程退出（{:?}）", exit.code()));
            },
            Ok(None) => {},
            Err(e) => {
                kill_child(&mut child);
                fail_pending(
                    &mut pending,
                    CommandOutcome::Unavailable(format!("等待进程失败: {e}")),
                );
                return InstanceEnd::Crashed(format!("等待进程失败: {e}"));
            },
        }

        // stdout 消息
        loop {
            match incoming_rx.try_recv() {
                Ok(HostIncoming::Result { id, ok, text }) => {
                    let text = sanitize_short(&text, MAX_RESULT_TEXT_CHARS);
                    if let Some(pending) = pending.remove(&id) {
                        if let Some(reply) = pending.reply {
                            let outcome = if ok {
                                CommandOutcome::Ok(text)
                            } else {
                                CommandOutcome::Error(text)
                            };
                            let _ = reply.send(outcome);
                        }
                    } else {
                        eprintln!("[plugin] {plugin}: 收到未知请求 id={id} 的结果，已忽略");
                    }
                },
                Ok(HostIncoming::Status { text, tooltip, command }) => {
                    // status 响应没有强制 id：移除最早的一个状态请求，避免 pending 堆积
                    // 导致每 2s 误报“状态请求超时”。
                    if let Some(id) = pending
                        .iter()
                        .find_map(|(id, request)| (request.kind == PendingKind::Status).then_some(*id))
                    {
                        pending.remove(&id);
                    }
                    status_errors = 0;
                    let text = sanitize_short(&text, MAX_STATUS_TEXT_CHARS);
                    let tooltip = sanitize_short(&tooltip, MAX_TOOLTIP_CHARS);
                    let command = if command.is_empty() {
                        String::new()
                    } else {
                        match normalize_status_command(package, &command) {
                            Some(command) => command,
                            None => {
                                eprintln!(
                                    "[plugin] {plugin}: 状态 command 不是本插件命令，已忽略: {}",
                                    sanitize_short(&command, 80)
                                );
                                String::new()
                            },
                        }
                    };
                    let changed = match status.lock() {
                        Ok(mut status) => {
                            let changed = status.text != text
                                || status.tooltip != tooltip
                                || status.command != command;
                            status.text = text;
                            status.tooltip = tooltip;
                            status.command = command;
                            changed
                        },
                        Err(_) => false,
                    };
                    if changed {
                        status_revision.fetch_add(1, Ordering::Relaxed);
                        notify_wake(wake);
                        eprintln!(
                            "[plugin] {plugin}: 状态更新 text={:?} tooltip={:?} command={:?}",
                            sanitize_short(&status.lock().map(|s| s.text.clone()).unwrap_or_default(), 120),
                            sanitize_short(&status.lock().map(|s| s.tooltip.clone()).unwrap_or_default(), 120),
                            sanitize_short(&status.lock().map(|s| s.command.clone()).unwrap_or_default(), 120),
                        );
                    }
                },
                Ok(HostIncoming::Log(text)) => {
                    eprintln!("[plugin] {plugin}: {}", sanitize_short(&text, 500));
                },
                Ok(HostIncoming::BadLine(line)) => {
                    bad_lines += 1;
                    eprintln!(
                        "[plugin] {plugin}: 坏 JSON/超长行（{}/{}）: {}",
                        bad_lines,
                        PROCESS_MAX_BAD_LINES,
                        sanitize_short(&line, 160)
                    );
                    if bad_lines >= PROCESS_MAX_BAD_LINES {
                        fail_pending(
                            &mut pending,
                            CommandOutcome::Unavailable("坏 JSON 过多，插件被禁用".to_string()),
                        );
                        kill_child(&mut child);
                        return InstanceEnd::Disabled("坏 JSON/超长行过多".to_string());
                    }
                },
                Ok(HostIncoming::Ready(_api)) => {},
                Ok(HostIncoming::Eof) => {
                    fail_pending(
                        &mut pending,
                        CommandOutcome::Unavailable("插件 stdout 关闭".to_string()),
                    );
                    return InstanceEnd::Crashed("stdout 关闭".to_string());
                },
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    fail_pending(
                        &mut pending,
                        CommandOutcome::Unavailable("stdout reader 退出".to_string()),
                    );
                    return InstanceEnd::Crashed("stdout reader 退出".to_string());
                },
            }
        }

        // 宿主请求（命令/事件/退出）
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(SupervisorMsg::Execute { command, args, reply }) => {
                let id = next_id;
                next_id = next_id.wrapping_add(1).max(1);
                let message = serde_json::json!({
                    "type": "command",
                    "id": id,
                    "command": command,
                    "args": args,
                });
                if let Err(e) = send_json_line(&mut stdin, &message) {
                    let _ = reply.send(CommandOutcome::Unavailable(format!("写入 stdin 失败: {e}")));
                    kill_child(&mut child);
                    fail_pending(&mut pending, CommandOutcome::Unavailable("插件 stdin 写入失败".to_string()));
                    return InstanceEnd::Crashed(format!("写入命令失败: {e}"));
                }
                pending.insert(
                    id,
                    PendingRequest {
                        kind: PendingKind::Command,
                        deadline: Instant::now() + command_timeout,
                        label: command,
                        reply: Some(reply),
                    },
                );
            },
            Ok(SupervisorMsg::Event { event, payload }) => {
                let message = serde_json::json!({
                    "type": "event",
                    "event": event.as_str(),
                    "payload": payload,
                });
                if let Err(e) = send_json_line(&mut stdin, &message) {
                    kill_child(&mut child);
                    fail_pending(&mut pending, CommandOutcome::Unavailable("插件 stdin 写入失败".to_string()));
                    return InstanceEnd::Crashed(format!("写入事件失败: {e}"));
                }
            },
            Ok(SupervisorMsg::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                fail_pending(&mut pending, CommandOutcome::Unavailable("宿主退出".to_string()));
                shutdown_child(&mut child, &mut stdin);
                return InstanceEnd::Shutdown;
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {},
        }

        // 请求超时：命令超时 kill 进程（按崩溃重启）；状态超时只丢弃该次请求。
        let now = Instant::now();
        let mut timed_out_command: Option<String> = None;
        let mut status_timed_out = 0u32;
        pending.retain(|_id, request| {
            if now < request.deadline {
                return true;
            }
            match request.kind {
                PendingKind::Command => {
                    timed_out_command = Some(request.label.clone());
                    if let Some(reply) = request.reply.as_ref() {
                        let _ = reply.send(CommandOutcome::Timeout);
                    }
                    false
                },
                PendingKind::Status => {
                    status_timed_out += 1;
                    false
                },
            }
        });
        if status_timed_out > 0 {
            status_errors += status_timed_out;
            eprintln!(
                "[plugin] {plugin}: 状态请求超时（连续 {status_errors} 次），本次丢弃"
            );
        }
        if let Some(command) = timed_out_command {
            kill_child(&mut child);
            fail_pending(
                &mut pending,
                CommandOutcome::Unavailable("插件已被 kill（命令超时）".to_string()),
            );
            return InstanceEnd::Crashed(format!(
                "命令 {command} 超时（{}ms），已 kill",
                command_timeout.as_millis()
            ));
        }

        // 周期性状态请求（插件也可以主动推送 status）
        if has_status && now >= status_due {
            let id = next_id;
            next_id = next_id.wrapping_add(1).max(1);
            let message = serde_json::json!({ "type": "status", "id": id });
            if let Err(e) = send_json_line(&mut stdin, &message) {
                kill_child(&mut child);
                fail_pending(&mut pending, CommandOutcome::Unavailable("插件 stdin 写入失败".to_string()));
                return InstanceEnd::Crashed(format!("写入状态请求失败: {e}"));
            }
            pending.insert(
                id,
                PendingRequest {
                    kind: PendingKind::Status,
                    deadline: Instant::now() + PROCESS_STATUS_TIMEOUT,
                    label: "status".to_string(),
                    reply: None,
                },
            );
            status_due = now + PROCESS_STATUS_INTERVAL;
        }
    }
}

/// 状态项 command 只接受本插件命令；裸命令 id 归一化为完整 ID（点击可直接执行）。
fn normalize_status_command(package: &PluginPackage, command: &str) -> Option<String> {
    if command.len() > MAX_STATUS_COMMAND_CHARS {
        return None;
    }
    package.manifest.commands.iter().find_map(|decl| {
        let full = package.full_command_id(&decl.id);
        (command == full || command == decl.id).then_some(full)
    })
}

fn fail_pending(pending: &mut HashMap<u64, PendingRequest>, outcome: CommandOutcome) {
    for (_, request) in pending.drain() {
        if let Some(reply) = request.reply {
            let _ = reply.send(outcome.clone());
        }
    }
}

/// 启动一个插件子进程：stdin/stdout 管道、stderr 持续写日志、stdout 逐行解析 JSON。
struct SpawnedChild {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    incoming_rx: Option<Receiver<HostIncoming>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

fn spawn_plugin_child(package: &PluginPackage) -> Result<SpawnedChild, String> {
    let mut command = ProcessCommand::new(&package.entry);
    command
        .current_dir(&package.dir)
        .env("TERVO_PLUGIN_NAME", &package.manifest.name)
        .env("TERVO_PLUGIN_ID", package.manifest.plugin_id())
        .env("TERVO_PLUGIN_API", PLUGIN_API_VERSION.to_string())
        .env("TERVO_PLUGIN_PROTOCOL", PLUGIN_PROTOCOL_VERSION.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // 父进程（Tervo）退出时内核给插件发 SIGTERM，避免留下孤儿进程。
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    // 刚写完的 entry 在部分文件系统上会短暂 ETXTBSY；和 oneshot 路径保持一致重试。
    let mut child = None;
    for attempt in 0..5 {
        match command.spawn() {
            Ok(spawned) => {
                child = Some(spawned);
                break;
            },
            Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => {
                std::thread::sleep(Duration::from_millis(20));
            },
            Err(e) => return Err(format!("无法启动 {}: {e}", package.entry.display())),
        }
    }
    let mut child = child.ok_or_else(|| "无法启动：重试后仍失败".to_string())?;
    let stdin = child.stdin.take().ok_or_else(|| "stdin 管道不可用".to_string())?;
    let stdout = child.stdout.take().ok_or_else(|| "stdout 管道不可用".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "stderr 管道不可用".to_string())?;

    let (incoming_tx, incoming_rx) = crossbeam_channel::bounded(64);
    let plugin = package.manifest.name.clone();
    let stdout_thread = std::thread::Builder::new()
        .name(format!("plugin-out-{plugin}"))
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_line_capped(&mut reader, MAX_PROTOCOL_LINE_BYTES) {
                    Ok(Some((line, truncated))) => {
                        if truncated {
                            let _ = incoming_tx
                                .send(HostIncoming::BadLine(format!("超长行 > {MAX_PROTOCOL_LINE_BYTES}B")));
                            continue;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        let msg = parse_host_incoming(&line);
                        if incoming_tx.send(msg).is_err() {
                            return;
                        }
                    },
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = incoming_tx.send(HostIncoming::Eof);
        });
    let stderr_thread = std::thread::Builder::new()
        .name(format!("plugin-err-{plugin}"))
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                match read_line_capped(&mut reader, MAX_LOG_BYTES) {
                    Ok(Some((line, _))) => {
                        if !line.trim().is_empty() {
                            eprintln!("[plugin] {plugin} stderr: {}", sanitize_short(&line, 500));
                        }
                    },
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });

    Ok(SpawnedChild {
        child: Some(child),
        stdin: Some(stdin),
        incoming_rx: Some(incoming_rx),
        stdout_thread: stdout_thread.ok(),
        stderr_thread: stderr_thread.ok(),
    })
}

/// 从 BufRead 读一行并限制最大字节数；超长行继续 drain 到换行，不无限分配。
fn read_line_capped<R: BufRead>(reader: &mut R, cap: usize) -> std::io::Result<Option<(String, bool)>> {
    let mut out: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let (chunk, consume, has_newline) = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                if out.is_empty() && !truncated {
                    return Ok(None);
                }
                break;
            }
            let newline = buf.iter().position(|b| *b == b'\n');
            let data_len = newline.unwrap_or(buf.len());
            let consume = newline.map(|i| i + 1).unwrap_or(buf.len());
            (buf[..data_len].to_vec(), consume, newline.is_some())
        };
        if out.len() < cap {
            let take = (cap - out.len()).min(chunk.len());
            out.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                truncated = true;
            }
        } else if !chunk.is_empty() {
            truncated = true;
        }
        reader.consume(consume);
        if has_newline {
            break;
        }
    }
    while out.last().is_some_and(|b| *b == b'\r') {
        out.pop();
    }
    Ok(Some((String::from_utf8_lossy(&out).into_owned(), truncated)))
}

fn parse_host_incoming(line: &str) -> HostIncoming {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => return HostIncoming::BadLine(format!("{e}: {}", sanitize_short(line, 120))),
    };
    let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
        return HostIncoming::BadLine(format!("缺少 type: {}", sanitize_short(line, 120)));
    };
    match kind {
        "ready" => {
            let api = value.get("api").and_then(|v| v.as_u64()).unwrap_or(PLUGIN_API_VERSION as u64);
            HostIncoming::Ready(api.min(u32::MAX as u64) as u32)
        },
        "result" => {
            let Some(id) = value.get("id").and_then(|v| v.as_u64()) else {
                return HostIncoming::BadLine("result 缺少 id".to_string());
            };
            let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
            let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
            HostIncoming::Result { id, ok, text: text.to_string() }
        },
        "status" => {
            let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let tooltip = value.get("tooltip").and_then(|v| v.as_str()).unwrap_or("");
            let command = value.get("command").and_then(|v| v.as_str()).unwrap_or("");
            HostIncoming::Status {
                text: text.to_string(),
                tooltip: tooltip.to_string(),
                command: command.to_string(),
            }
        },
        "log" => HostIncoming::Log(
            value.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ),
        other => HostIncoming::BadLine(format!("未知 type {other:?}")),
    }
}

fn send_json_line(
    stdin: &mut std::process::ChildStdin,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn shutdown_child(child: &mut Child, stdin: &mut std::process::ChildStdin) {
    let _ = send_json_line(stdin, &serde_json::json!({ "type": "shutdown" }));
    // 关闭 stdin 让插件读到 EOF；最多等 300ms 自主退出，再 kill。
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    kill_child(child);
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
        let plugin_id = manifest.plugin_id().to_string();
        if self
            .packages
            .iter()
            .any(|p| p.manifest.plugin_id() == plugin_id)
        {
            self.skipped.push((
                manifest_path.display().to_string(),
                format!("重复插件 id {plugin_id}"),
            ));
            return;
        }
        let (theme_path, theme_error) = match manifest.contributes.theme.as_deref() {
            Some(theme) if !theme.trim().is_empty() => {
                match resolve_theme_file(&canonical_dir, theme) {
                    Ok(path) => (Some(path), None),
                    // 主题是可选贡献：坏主题只记录原因，插件本身仍加载。
                    Err(e) => (None, Some(e)),
                }
            },
            _ => (None, None),
        };
        self.packages.push(PluginPackage {
            manifest,
            dir: canonical_dir,
            entry,
            theme_path,
            theme_error,
        });
    }

    /// 设置页列表：旧清单 + v1 目录插件，按名称排序。
    pub fn display_lines(&self) -> Vec<String> {
        let mut rows: Vec<(String, String)> = self
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.display_line()))
            .chain(self.packages.iter().map(|p| {
                let runtime = if p.manifest.is_process() {
                    self.runtime_info(&p.manifest.name)
                } else {
                    None
                };
                (
                    p.manifest.name.clone(),
                    p.display_line_with(runtime.as_ref()),
                )
            }))
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
        let shared = Arc::new(wake);
        if let Ok(mut log) = self.activity.lock() {
            log.set_wake_arc(shared.clone());
        }
        if let Ok(mut guard) = self.wake.lock() {
            *guard = Some(shared);
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
            if package.manifest.is_process() {
                match self.process_handle(&plugin_name) {
                    Some(handle) if handle.tx.send(SupervisorMsg::Event {
                        event,
                        payload: payload.clone(),
                    }).is_ok() => {
                        spawned += 1;
                    },
                    Some(_) => {
                        eprintln!("[plugin] {plugin_name}: 常驻进程已退出，事件 {} 未投递", event.as_str());
                    },
                    None => {
                        eprintln!("[plugin] {plugin_name}: 常驻进程未启动，事件 {} 未投递", event.as_str());
                    },
                }
                continue;
            }
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
        if package.manifest.is_process() {
            return self.spawn_process_command(package, decl);
        }
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
                id: String::new(),
                name: "broken".to_string(),
                version: "1".to_string(),
                api: PLUGIN_API_VERSION,
                runtime: None,
                entry: "nope.sh".to_string(),
                enabled: true,
                description: String::new(),
                commands: vec![PluginCommandDecl {
                    id: "boom".to_string(),
                    title: "Boom".to_string(),
                    key: None,
                    args: Vec::new(),
                }],
                events: Vec::new(),
                permissions: PluginPermissions::default(),
                contributes: PluginContributes::default(),
            },
            dir: dir.clone(),
            entry: dir.join("nope.sh"),
            theme_path: None,
            theme_error: None,
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

    // ---------------------------------------------------------- 常驻进程插件（V1）

    const PROCESS_MANIFEST: &str = r#"
id = "dev.tervo.fake"
name = "fake"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"
description = "fake process plugin"
events = ["startup"]

[permissions]
filesystem = "none"
network = false
process = false
clipboard = false
credentials = "none"

[contributes]
status = true
commands = ["greet", "fail"]
theme = "theme.toml"

[[commands]]
id = "greet"
title = "Fake: Greet"
key = "ctrl+alt+g"
args = ["world"]

[[commands]]
id = "fail"
title = "Fake: Fail"
"#;

    const PROCESS_PY: &str = r#"#!/usr/bin/env python3
import json
import sys

for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        print("not json", flush=True)
        continue
    kind = msg.get("type")
    if kind == "init":
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
    elif kind == "command":
        cmd = msg.get("command")
        if cmd == "greet":
            text = "hello " + " ".join(msg.get("args", []))
            print(json.dumps({"type": "result", "id": msg["id"], "ok": True, "text": text}), flush=True)
        elif cmd == "fail":
            print(json.dumps({"type": "result", "id": msg["id"], "ok": False, "text": "nope"}), flush=True)
    elif kind == "status":
        print(json.dumps({"type": "status", "text": "build ok", "tooltip": "fake status", "command": "greet"}), flush=True)
    elif kind == "event":
        with open("events.log", "a", encoding="utf-8") as fh:
            fh.write(msg.get("event", "") + "\n")
    elif kind == "shutdown":
        break
"#;

    fn write_process_package(
        root: &Path,
        dir_name: &str,
        manifest: &str,
        script_name: &str,
        script: &str,
    ) -> PathBuf {
        let dir = write_package(root, dir_name, manifest, None);
        write_script(&dir.join(script_name), script);
        dir
    }

    fn wait_phase(store: &PluginStore, name: &str, phase: RuntimePhase, timeout: Duration) -> bool {
        wait_until(
            || store.runtime_info(name).is_some_and(|info| info.phase == phase),
            timeout,
        )
    }

    #[cfg(unix)]
    #[test]
    fn process_plugin_handshakes_runs_commands_and_events() {
        let dir = tmpdir("process-cmd");
        write_process_package(&dir, "fake", PROCESS_MANIFEST, "plugin.py", PROCESS_PY);
        let mut store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        assert_eq!(store.packages.len(), 1);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "fake", RuntimePhase::Running, Duration::from_secs(4)),
            "插件未就绪: {:?}",
            store.runtime_info("fake")
        );

        assert!(store.spawn_command("plugin.fake.greet"));
        assert!(
            wait_until(
                || store
                    .activity_snapshot()
                    .1
                    .iter()
                    .any(|line| line.contains("command greet") && line.contains("hello world")),
                Duration::from_secs(4)
            ),
            "命令结果未记录: {:?}",
            store.activity_snapshot().1
        );
        let notices = store.take_notices();
        assert!(
            notices.iter().any(|n| n.plugin == "fake" && n.ok && n.text.contains("hello world")),
            "{notices:?}"
        );

        // 静态参数 + 返回 ok=false：结果文本回 UI 通知/活动，但标记 error。
        assert!(store.spawn_command("plugin.fake.fail"));
        assert!(
            wait_until(
                || store.take_notices().iter().any(|n| !n.ok && n.text.contains("nope")),
                Duration::from_secs(4)
            ),
            "失败结果未通知"
        );

        // 常驻进程的事件是通知，不结束进程；插件写入自己的 events.log。
        assert_eq!(store.spawn_event(PluginEvent::Startup, startup_payload()), 1);
        assert!(
            wait_until(
                || read_lines(&dir.join("fake").join("events.log"))
                    .iter()
                    .any(|line| line == "startup"),
                Duration::from_secs(4)
            ),
            "事件未投递"
        );
        assert!(store.runtime_info("fake").is_some_and(|i| i.is_healthy()));
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn process_plugin_status_provider_is_polled_and_command_normalized() {
        let dir = tmpdir("process-status");
        write_process_package(&dir, "fake", PROCESS_MANIFEST, "plugin.py", PROCESS_PY);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "fake", RuntimePhase::Running, Duration::from_secs(4)),
            "{:?}",
            store.runtime_info("fake")
        );
        let mut items = Vec::new();
        assert!(
            wait_until(
                || {
                    let (_, snapshot) = store.status_snapshot();
                    if snapshot.is_empty() {
                        false
                    } else {
                        items = snapshot;
                        true
                    }
                },
                Duration::from_secs(4)
            ),
            "状态未被轮询: {:?}",
            store.runtime_info("fake")
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].plugin, "fake");
        assert_eq!(items[0].text, "build ok");
        assert_eq!(items[0].tooltip, "fake status");
        // 裸命令 id 归一化为完整 ID，状态栏点击才能走 run_command。
        assert_eq!(items[0].command, "plugin.fake.greet");
        let (rev, _) = store.status_snapshot();
        assert!(rev > 0);
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn node_process_plugin_command_result() {
        if std::process::Command::new("node").arg("--version").output().is_err() {
            eprintln!("跳过：node 不可用");
            return;
        }
        let manifest = r#"
name = "nodefake"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.js"

[[commands]]
id = "greet"
title = "Node: Greet"
"#;
        let script = r#"#!/usr/bin/env node
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let m;
  try { m = JSON.parse(line); } catch (e) { return; }
  if (m.type === 'init') {
    console.log(JSON.stringify({ type: 'ready', api: 1 }));
  } else if (m.type === 'command') {
    console.log(JSON.stringify({ type: 'result', id: m.id, ok: true, text: 'node:' + m.command }));
  }
});
"#;
        let dir = tmpdir("process-node");
        write_process_package(&dir, "nodefake", manifest, "plugin.js", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "nodefake", RuntimePhase::Running, Duration::from_secs(4)),
            "{:?}",
            store.runtime_info("nodefake")
        );
        assert!(store.spawn_command("plugin.nodefake.greet"));
        assert!(
            wait_until(
                || store
                    .activity_snapshot()
                    .1
                    .iter()
                    .any(|line| line.contains("node:greet")),
                Duration::from_secs(4)
            ),
            "{:?}",
            store.activity_snapshot().1
        );
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn crashing_process_plugin_restarts_then_disables() {
        let manifest = r#"
name = "crash"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"

[[commands]]
id = "ping"
title = "Crash: Ping"
"#;
        let script = r#"#!/usr/bin/env python3
import json
import sys
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") == "init":
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
        sys.exit(1)
"#;
        let dir = tmpdir("process-crash");
        write_process_package(&dir, "crash", manifest, "plugin.py", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "crash", RuntimePhase::Disabled, Duration::from_secs(8)),
            "崩溃插件未按上限禁用: {:?}",
            store.runtime_info("crash")
        );
        let info = store.runtime_info("crash").unwrap();
        assert!(info.error.contains("重启上限") || info.error.contains("达到重启上限"), "{info:?}");
        let line = store.display_lines()[0].clone();
        assert!(line.contains("disabled") && line.contains("重启上限"), "{line}");
        // 已禁用插件的命令不再可执行（find_command 过滤 enabled，但插件进程状态独立）。
        assert!(!store.spawn_command("no.such.command"));
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn command_timeout_kills_process_and_reports_timeout() {
        let manifest = r#"
name = "slow"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"

[[commands]]
id = "slow"
title = "Slow: Run"
"#;
        let script = r#"#!/usr/bin/env python3
import json
import sys
import time
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") == "init":
        print(json.dumps({"type": "ready", "api": 1}), flush=True)
    elif msg.get("type") == "command":
        time.sleep(30)
"#;
        let dir = tmpdir("process-timeout");
        write_process_package(&dir, "slow", manifest, "plugin.py", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_millis(300));
        assert!(
            wait_phase(&store, "slow", RuntimePhase::Running, Duration::from_secs(4)),
            "{:?}",
            store.runtime_info("slow")
        );
        assert!(store.spawn_command("plugin.slow.slow"));
        assert!(
            wait_until(
                || store
                    .activity_snapshot()
                    .1
                    .iter()
                    .any(|line| line.contains("timeout") && line.contains("命令超时")),
                Duration::from_secs(4)
            ),
            "{:?}",
            store.activity_snapshot().1
        );
        let notices = store.take_notices();
        assert!(notices.iter().any(|n| n.text.contains("超时")), "{notices:?}");
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn bad_json_disables_plugin_without_hanging_host() {
        let manifest = r#"
name = "badjson"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"

[[commands]]
id = "ping"
title = "Bad: Ping"
"#;
        let script = r#"#!/usr/bin/env python3
import json
import sys
import time
print(json.dumps({"type": "ready", "api": 1}), flush=True)
for _ in range(10):
    print("this is not json", flush=True)
time.sleep(30)
"#;
        let dir = tmpdir("process-badjson");
        write_process_package(&dir, "badjson", manifest, "plugin.py", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "badjson", RuntimePhase::Disabled, Duration::from_secs(6)),
            "坏 JSON 未禁用: {:?}",
            store.runtime_info("badjson")
        );
        let info = store.runtime_info("badjson").unwrap();
        assert!(info.error.contains("坏 JSON"), "{info:?}");
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn handshake_timeout_is_reported_and_retried() {
        let manifest = r#"
name = "dumb"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"

[[commands]]
id = "ping"
title = "Dumb: Ping"
"#;
        let script = "#!/usr/bin/env python3\nimport time\ntime.sleep(30)\n";
        let dir = tmpdir("process-handshake");
        write_process_package(&dir, "dumb", manifest, "plugin.py", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_until(
                || {
                    store.runtime_info("dumb").is_some_and(|info| {
                        info.phase == RuntimePhase::Restarting
                            && info.error.contains("握手超时")
                    })
                },
                Duration::from_secs(5)
            ),
            "握手超时未上报: {:?}",
            store.runtime_info("dumb")
        );
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn disabled_process_plugin_is_not_started() {
        let manifest = PROCESS_MANIFEST.replace(
            "description = \"fake process plugin\"",
            "description = \"fake process plugin\"\nenabled = false",
        );
        let dir = tmpdir("process-disabled");
        write_process_package(&dir, "fake", &manifest, "plugin.py", PROCESS_PY);
        let mut store = PluginStore::load_from(&dir);
        assert_eq!(store.packages.len(), 1);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(store.runtime_info("fake").is_none());
        assert!(!store.spawn_command("plugin.fake.greet"));
        assert!(store.display_lines()[0].contains("disabled"), "{:?}", store.display_lines());
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_manifest_parses_permissions_contributions_and_theme() {
        let dir = tmpdir("process-manifest");
        // 在既有 [permissions] 表里换非默认值，验证声明+展示不会被拒绝。
        let manifest = PROCESS_MANIFEST.replace(
            "filesystem = \"none\"\nnetwork = false\nprocess = false\nclipboard = false\ncredentials = \"none\"",
            "filesystem = \"read-plugin\"\nnetwork = true\nprocess = true\nclipboard = true\ncredentials = \"profile-ref\"",
        );
        write_process_package(&dir, "fake", &manifest, "plugin.py", PROCESS_PY);
        std::fs::write(dir.join("fake").join("theme.toml"), ALACRITTY_THEME).unwrap();
        let store = PluginStore::load_from(&dir);
        assert!(store.skipped.is_empty(), "{:?}", store.skipped);
        let package = &store.packages[0];
        assert_eq!(package.manifest.plugin_id(), "dev.tervo.fake");
        assert_eq!(package.manifest.runtime_kind(), Ok(PluginRuntimeKind::Process));
        assert!(package.manifest.contributes.status);
        assert_eq!(package.manifest.commands[0].args, vec!["world".to_string()]);
        assert!(package.theme_path.is_some());
        let perms = package.manifest.permissions.summary();
        assert!(perms.contains("read-plugin") && perms.contains("net=true"), "{perms}");
        assert!(perms.contains("仅声明/未授予"), "{perms}");
        let line = store.display_lines()[0].clone();
        assert!(line.contains("process"), "{line}");
        assert!(line.contains("status"), "{line}");
        assert!(line.contains("theme"), "{line}");
        assert!(line.contains("仅声明/未授予"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_process_manifest_fields_are_skipped() {
        let cases = [
            ("runtime", "runtime = \"process\"", "runtime = \"wasm\"", "runtime"),
            (
                "status-oneshot",
                "runtime = \"process\"",
                "runtime = \"oneshot\"",
                "contributes.status",
            ),
            (
                "unknown-contrib",
                "commands = [\"greet\", \"fail\"]",
                "commands = [\"greet\", \"missing\"]",
                "contributes.commands",
            ),
            (
                "bad-permission",
                "filesystem = \"none\"",
                "filesystem = \"everything\"",
                "filesystem",
            ),
            (
                "theme-escape",
                "theme = \"theme.toml\"",
                "theme = \"../theme.toml\"",
                "theme",
            ),
            (
                "theme-absolute",
                "theme = \"theme.toml\"",
                "theme = \"/tmp/theme.toml\"",
                "绝对路径",
            ),
        ];
        for (tag, from, to, needle) in cases {
            let dir = tmpdir(&format!("process-invalid-{tag}"));
            let manifest = PROCESS_MANIFEST.replace(from, to);
            write_process_package(&dir, "fake", &manifest, "plugin.py", PROCESS_PY);
            let store = PluginStore::load_from(&dir);
            assert!(store.packages.is_empty(), "{tag}: {:?}", store.display_lines());
            assert_eq!(store.skipped.len(), 1, "{tag}: {:?}", store.skipped);
            assert!(
                store.skipped[0].1.contains(needle),
                "{tag}: {:?}",
                store.skipped
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn missing_plugin_theme_keeps_plugin_and_reports_reason() {
        let dir = tmpdir("process-theme-missing");
        write_process_package(&dir, "fake", PROCESS_MANIFEST, "plugin.py", PROCESS_PY);
        let store = PluginStore::load_from(&dir);
        assert_eq!(store.packages.len(), 1);
        let package = &store.packages[0];
        assert!(package.theme_path.is_none());
        assert!(package.theme_error.as_deref().unwrap_or("").contains("主题文件不存在"));
        let line = &store.display_lines()[0];
        assert!(line.contains("theme-error"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const ALACRITTY_THEME: &str = r##"
[colors.primary]
background = "#101010"
foreground = "#eeeeee"

[colors.cursor]
cursor = "#ffffff"

[colors.normal]
black = "#000000"
red = "#cc0000"
green = "#00cc00"
yellow = "#cccc00"
blue = "#0000cc"
magenta = "#cc00cc"
cyan = "#00cccc"
white = "#cccccc"

[colors.bright]
black = "#555555"
red = "#ff5555"
green = "#55ff55"
yellow = "#ffff55"
blue = "#5555ff"
magenta = "#ff55ff"
cyan = "#55ffff"
white = "#ffffff"
"##;

    #[test]
    fn read_line_capped_handles_crlf_eof_and_overlong_lines() {
        use std::io::Cursor;
        let mut input = Cursor::new(b"one\r\ntwo\nthreee".to_vec());
        assert_eq!(
            read_line_capped(&mut input, 16).unwrap(),
            Some(("one".to_string(), false))
        );
        assert_eq!(
            read_line_capped(&mut input, 16).unwrap(),
            Some(("two".to_string(), false))
        );
        assert_eq!(
            read_line_capped(&mut input, 16).unwrap(),
            Some(("threee".to_string(), false))
        );
        assert_eq!(read_line_capped(&mut input, 16).unwrap(), None);

        // cap 以内的前缀 + truncated；后续行仍可继续读，内存有界。
        let mut input = Cursor::new(b"0123456789abcdefXYZ\nok\n".to_vec());
        assert_eq!(
            read_line_capped(&mut input, 8).unwrap(),
            Some(("01234567".to_string(), true))
        );
        assert_eq!(
            read_line_capped(&mut input, 8).unwrap(),
            Some(("ok".to_string(), false))
        );
        assert_eq!(read_line_capped(&mut input, 8).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn process_plugin_api_mismatch_is_disabled() {
        let manifest = r#"
name = "apimismatch"
version = "1.0.0"
api = 1
runtime = "process"
entry = "plugin.py"

[[commands]]
id = "ping"
title = "Api: Ping"
"#;
        let script = r#"#!/usr/bin/env python3
import json
print(json.dumps({"type": "ready", "api": 2}), flush=True)
import time
time.sleep(30)
"#;
        let dir = tmpdir("process-api-mismatch");
        write_process_package(&dir, "apimismatch", manifest, "plugin.py", script);
        let mut store = PluginStore::load_from(&dir);
        store.start_process_plugins_with_timeout(Duration::from_secs(2));
        assert!(
            wait_phase(&store, "apimismatch", RuntimePhase::Disabled, Duration::from_secs(4)),
            "{:?}",
            store.runtime_info("apimismatch")
        );
        let info = store.runtime_info("apimismatch").unwrap();
        assert!(info.error.contains("握手 api=2"), "{info:?}");
        store.shutdown_processes();
        let _ = std::fs::remove_dir_all(&dir);
    }

}
