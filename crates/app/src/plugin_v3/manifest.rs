// SPDX-License-Identifier: GPL-3.0-only
//! Tervo Plugin API v3 manifest。
//!
//! v3 是唯一受支持的插件协议：`api = 3` 且 `host_abi = 3`。v1/v2 清单不会进入
//! 任何执行路径，只会在设置页以 `unsupported` 展示并给出迁移提示。
//!
//! 这里只做**纯数据解析 + 字段校验**；文件系统边界（entry/theme 必须留在插件
//! 目录内、必须可执行）由 [`resolve_plugin_path`] / [`validate_entry`] 负责。
//! 权限字段是给宿主 mediated API 用的 capability 声明，不是 OS 沙箱：插件进程
//! 仍以当前用户身份运行，拥有该用户的全部系统权限。

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 唯一支持的插件 API 版本。
pub const PLUGIN_API_VERSION: u32 = 3;
/// 唯一支持的宿主 ABI。
pub const HOST_ABI: u32 = 3;

/// 清单文件读取上限，防止把大文件当 TOML 解析。
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// entry/theme 路径长度上限。
pub const MAX_PATH_CHARS: usize = 512;
/// id/命令 id 的长度上限。
pub const MAX_ID_CHARS: usize = 64;

/// 宿主生命周期事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginEvent {
    Startup,
    TabOpen,
    TabClose,
    PaneFocus,
}

impl PluginEvent {
    /// 全部支持的事件（文档/校验/测试共用）。
    pub const ALL: [PluginEvent; 4] = [
        PluginEvent::Startup,
        PluginEvent::TabOpen,
        PluginEvent::TabClose,
        PluginEvent::PaneFocus,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PluginEvent::Startup => "startup",
            PluginEvent::TabOpen => "tab-open",
            PluginEvent::TabClose => "tab-close",
            PluginEvent::PaneFocus => "pane-focus",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL.into_iter().find(|event| event.as_str() == value)
    }
}

/// `[permissions]`：声明插件希望使用的宿主能力。
///
/// 宿主能真正 gate 的是宿主 mediated API（terminal_context / terminal_write /
/// clipboard）。`filesystem`、`network`、`process`、`credentials` 只是声明和
/// 用户确认时展示的信息，Tervo 没有 OS 级沙箱去阻止插件直接使用这些能力。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    /// `none` | `plugin` | `home`（声明；宿主不提供文件 API）。
    pub filesystem: String,
    /// 声明插件会访问网络（宿主不代理网络，也不阻断）。
    pub network: bool,
    /// 声明插件会启动子进程（宿主不代理，也不阻断）。
    pub process: bool,
    /// `clipboard.read` / `clipboard.write` 宿主请求的 capability gate。
    pub clipboard: bool,
    /// `terminal.read` 宿主请求的 capability gate（每次读取还要 ask/allow）。
    pub terminal_context: bool,
    /// `terminal.write` 宿主请求的 capability gate（每次写入还要 ask/allow）。
    pub terminal_write: bool,
    /// `none` | `own`（own = 插件自行管理凭据；宿主永不提供 Vault/环境变量明文）。
    pub credentials: String,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            filesystem: "none".to_string(),
            network: false,
            process: false,
            clipboard: false,
            terminal_context: false,
            terminal_write: false,
            credentials: "none".to_string(),
        }
    }
}

impl Permissions {
    pub fn validate(&self) -> Result<(), String> {
        match self.filesystem.trim() {
            "none" | "plugin" | "home" => {},
            other => {
                return Err(format!(
                    "permissions.filesystem 只支持 none/plugin/home（当前 {other:?}）"
                ))
            },
        }
        match self.credentials.trim() {
            "none" | "own" => {},
            other => {
                return Err(format!(
                    "permissions.credentials 只支持 none/own（当前 {other:?}）；宿主不提供凭据明文"
                ))
            },
        }
        Ok(())
    }

    /// 首次启用确认、持久化指纹、capability gate 共用的规范化权限集合。
    ///
    /// 顺序固定；空集合表示插件没有声明任何权限。
    pub fn names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let fs = self.filesystem.trim();
        if fs != "none" {
            names.push(format!("filesystem={fs}"));
        }
        if self.network {
            names.push("network".to_string());
        }
        if self.process {
            names.push("process".to_string());
        }
        if self.clipboard {
            names.push("clipboard".to_string());
        }
        if self.terminal_context {
            names.push("terminal_context".to_string());
        }
        if self.terminal_write {
            names.push("terminal_write".to_string());
        }
        let cred = self.credentials.trim();
        if cred != "none" {
            names.push(format!("credentials={cred}"));
        }
        names
    }

    pub fn declares_anything(&self) -> bool {
        !self.names().is_empty()
    }

    /// 设置页/确认弹窗展示用摘要（英文 key=value，不改写第三方声明）。
    pub fn summary(&self) -> String {
        let names = self.names();
        if names.is_empty() {
            "none declared".to_string()
        } else {
            names.join(", ")
        }
    }

    /// 宿主 mediated API 的运行时 gate。
    pub fn allows(&self, capability: Capability) -> bool {
        match capability {
            Capability::Clipboard => self.clipboard,
            Capability::TerminalContext => self.terminal_context,
            Capability::TerminalWrite => self.terminal_write,
        }
    }
}

/// 宿主真正执行 capability gate 的能力。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Clipboard,
    TerminalContext,
    TerminalWrite,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Clipboard => "clipboard",
            Capability::TerminalContext => "terminal_context",
            Capability::TerminalWrite => "terminal_write",
        }
    }
}

/// `[contributes.assistant]`：v3 MVP 只做占位/校验，不实现 AI provider。
///
/// 允许写成 bool 或表，宿主只展示声明，不发送任何 AI 请求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AssistantContribution {
    Enabled(bool),
    Meta(AssistantMeta),
}

impl AssistantContribution {
    pub fn is_enabled(&self) -> bool {
        match self {
            AssistantContribution::Enabled(enabled) => *enabled,
            AssistantContribution::Meta(_) => true,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            AssistantContribution::Enabled(_) => Ok(()),
            AssistantContribution::Meta(meta) => meta.validate(),
        }
    }
}

/// `[contributes.assistant]` 表形式；只允许元数据，避免变成 provider 配置。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssistantMeta {
    pub name: String,
    pub description: String,
}

impl AssistantMeta {
    fn validate(&self) -> Result<(), String> {
        if self.name.chars().count() > 128 {
            return Err("contributes.assistant.name 超过 128 字符".into());
        }
        if self.description.chars().count() > 512 {
            return Err("contributes.assistant.description 超过 512 字符".into());
        }
        Ok(())
    }
}

/// `[contributes]`：声明式贡献点。所有 UI 都由宿主渲染，插件不能注入 Slint。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Contributes {
    /// 有 `[[commands]]` 时必须为 true；false 时不允许声明命令。
    pub commands: bool,
    /// 允许宿主周期调用 `status.get`，也允许插件推送 `ui.status`。
    pub status: bool,
    /// 订阅的生命周期事件。
    pub events: Vec<String>,
    /// 插件目录内相对路径的声明式 TOML 主题。
    pub theme: Option<String>,
    /// 插件可渲染的声明式面板 ID；宿主只渲染固定元素类型。
    pub panels: Vec<String>,
    /// 允许宿主请求 `settings.get` 并渲染设置 schema。
    pub settings: bool,
    /// v3 MVP 占位；声明了也不会启动 AI provider。
    pub assistant: Option<AssistantContribution>,
}


impl Contributes {
    pub fn validate(&self, commands: &[CommandDecl]) -> Result<(), String> {
        if self.commands && commands.is_empty() {
            return Err("contributes.commands = true 但没有 [[commands]]".into());
        }
        if !self.commands && !commands.is_empty() {
            return Err("声明了 [[commands]] 但 contributes.commands 不是 true".into());
        }
        let mut seen_events = Vec::new();
        for raw in &self.events {
            let event = PluginEvent::parse(raw)
                .ok_or_else(|| format!("contributes.events 含未知事件 {raw:?}"))?;
            if seen_events.contains(&event) {
                return Err(format!("contributes.events 重复 {raw:?}"));
            }
            seen_events.push(event);
        }
        if let Some(theme) = &self.theme {
            validate_relative_path(theme).map_err(|e| format!("contributes.theme 非法: {e}"))?;
        }
        let mut seen_panels = Vec::new();
        if self.panels.len() > 16 {
            return Err("contributes.panels 最多 16 个".into());
        }
        for panel in &self.panels {
            validate_id(panel, "contributes.panels id")?;
            if seen_panels.iter().any(|seen| seen == panel) {
                return Err(format!("contributes.panels 重复 {panel:?}"));
            }
            seen_panels.push(panel.clone());
        }
        if let Some(assistant) = &self.assistant {
            assistant.validate()?;
        }
        Ok(())
    }

    pub fn declares_event(&self, event: PluginEvent) -> bool {
        self.events
            .iter()
            .any(|raw| PluginEvent::parse(raw) == Some(event))
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant
            .as_ref()
            .is_some_and(AssistantContribution::is_enabled)
    }
}

/// `[[commands]]`：声明式命令；宿主负责 Palette 展示与调用。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

impl CommandDecl {
    fn validate(&self) -> Result<(), String> {
        validate_id(&self.id, "commands.id")?;
        if self.title.trim().is_empty() {
            return Err(format!("commands.{} 缺少 title", self.id));
        }
        if self.title.chars().count() > 120 {
            return Err(format!("commands.{} title 超过 120 字符", self.id));
        }
        if self.description.chars().count() > 512 {
            return Err(format!("commands.{} description 超过 512 字符", self.id));
        }
        Ok(())
    }
}

/// 完整 v3 清单。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub api: u32,
    pub host_abi: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    /// v3 只支持 `process`（常驻子进程 + JSON-RPC 2.0 line-delimited stdio）。
    pub runtime: String,
    /// 相对插件目录的执行入口。
    pub entry: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub contributes: Contributes,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

impl Manifest {
    pub fn validate(&self) -> Result<(), String> {
        if self.api != PLUGIN_API_VERSION {
            return Err(format!(
                "不支持的 api {}（v3 宿主只加载 api = {}；v1/v2 已 unsupported）",
                self.api, PLUGIN_API_VERSION
            ));
        }
        if self.host_abi != HOST_ABI {
            return Err(format!(
                "不支持的 host_abi {}（需要 host_abi = {HOST_ABI}）",
                self.host_abi
            ));
        }
        validate_id(&self.id, "id")?;
        let name = self.name.trim();
        if name.is_empty() {
            return Err("缺少 name".into());
        }
        if name.chars().count() > 128 {
            return Err("name 超过 128 字符".into());
        }
        if self.version.trim().is_empty() {
            return Err("缺少 version".into());
        }
        if self.version.chars().count() > 64 {
            return Err("version 超过 64 字符".into());
        }
        if self.description.chars().count() > 512 {
            return Err("description 超过 512 字符".into());
        }
        if self.runtime.trim() != "process" {
            return Err(format!(
                "v3 只支持 runtime = \"process\"（当前 {:?}；oneshot 属于已移除的 v1/v2）",
                self.runtime
            ));
        }
        validate_relative_path(&self.entry)?;
        self.permissions.validate()?;
        self.contributes.validate(&self.commands)?;
        let mut seen = Vec::new();
        for command in &self.commands {
            command.validate()?;
            if seen.iter().any(|id| id == &command.id) {
                return Err(format!("commands.id 重复 {:?}", command.id));
            }
            seen.push(command.id.clone());
        }
        Ok(())
    }

    pub fn full_command_id(&self, command: &str) -> String {
        format!("plugin.{}.{}", self.id, command)
    }

    pub fn event_enabled(&self, event: PluginEvent) -> bool {
        self.contributes.declares_event(event)
    }

    /// 权限集合的规范化指纹；用于判断是否需要重新确认。
    pub fn permission_fingerprint(&self) -> Vec<String> {
        self.permissions.names()
    }

    pub fn panel(&self, id: &str) -> bool {
        self.contributes.panels.iter().any(|panel| panel == id)
    }
}

/// 校验稳定 ID：ASCII 字母/数字开头，只允许 `[A-Za-z0-9._-]`。
pub fn validate_id(value: &str, what: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{what} 不能为空"));
    }
    if value.chars().count() > MAX_ID_CHARS {
        return Err(format!("{what} 超过 {MAX_ID_CHARS} 字符"));
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap_or(' ');
    if !first.is_ascii_alphanumeric() {
        return Err(format!("{what} 必须以 ASCII 字母或数字开头（当前 {value:?}）"));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "{what} 只允许 ASCII 字母/数字/./_/-（当前 {value:?}）"
        ));
    }
    Ok(())
}

/// 只允许插件目录内的相对路径；拒绝绝对路径、`..`、空段和反斜杠。
pub fn validate_relative_path(value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("路径不能为空".into());
    }
    if value.chars().count() > MAX_PATH_CHARS {
        return Err(format!("路径超过 {MAX_PATH_CHARS} 字符"));
    }
    if value.contains('\\') {
        return Err("路径不能包含反斜杠".into());
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err("不允许绝对路径".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {},
            Component::CurDir => return Err("路径不能包含 . 段".into()),
            Component::ParentDir => return Err("路径不能包含 ..".into()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("不允许绝对路径".into())
            },
        }
    }
    Ok(())
}

/// 把清单里的相对 entry/theme 解析成插件目录内的真实路径。
///
/// 先 canonicalize 插件目录与目标，再确认目标仍在插件目录内，防止符号链接
/// 逃逸。`expect_file` 为 true 时要求目标是普通文件。
pub fn resolve_plugin_path(
    plugin_dir: &Path,
    relative: &str,
    expect_file: bool,
) -> Result<PathBuf, String> {
    validate_relative_path(relative)?;
    let canonical_dir = plugin_dir
        .canonicalize()
        .map_err(|e| format!("插件目录不可用 {}: {e}", plugin_dir.display()))?;
    let joined = canonical_dir.join(relative.trim());
    let resolved = joined
        .canonicalize()
        .map_err(|e| format!("路径不存在 {}: {e}", joined.display()))?;
    if !resolved.starts_with(&canonical_dir) {
        return Err(format!(
            "路径逃逸插件目录: {}（插件目录 {}）",
            resolved.display(),
            canonical_dir.display()
        ));
    }
    if expect_file && !resolved.is_file() {
        return Err(format!("不是普通文件: {}", resolved.display()));
    }
    Ok(resolved)
}

/// 入口额外要求：可执行（Unix mode 有执行位）。
pub fn resolve_entry(plugin_dir: &Path, entry: &str) -> Result<PathBuf, String> {
    let resolved = resolve_plugin_path(plugin_dir, entry, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&resolved)
            .map_err(|e| format!("读取 entry 权限失败 {}: {e}", resolved.display()))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(format!("entry 不可执行（缺少执行位）: {}", resolved.display()));
        }
    }
    Ok(resolved)
}

/// 从清单文本里只探测 `api`，用于把旧清单归类为 unsupported 而不是 invalid。
pub fn probe_api(value: &toml::Value) -> Option<u64> {
    value.get("api").and_then(toml::Value::as_integer).map(|api| api as u64)
}

/// 从旧清单文本里尽量提取展示用的 name/version。
pub fn legacy_display(value: &toml::Value, dir_name: &str) -> (String, String) {
    let name = value
        .get("name")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(dir_name)
        .to_string();
    let version = value
        .get("version")
        .and_then(toml::Value::as_str)
        .unwrap_or("?")
        .to_string();
    (name, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
api = 3
host_abi = 3
id = "dev.tervo.echo"
name = "Echo"
version = "1.0.0"
runtime = "process"
entry = "main.py"

[permissions]
filesystem = "plugin"
network = false
process = false
clipboard = true
terminal_context = true
terminal_write = false
credentials = "own"

[contributes]
commands = true
status = true
events = ["startup", "tab-open", "pane-focus"]
theme = "theme.toml"
panels = ["main"]
settings = true
assistant = false

[[commands]]
id = "greet"
title = "Greet"
"#;

    fn parse(text: &str) -> Manifest {
        toml::from_str(text).expect("manifest parses")
    }

    #[test]
    fn v3_manifest_parses_and_validates() {
        let manifest = parse(GOOD);
        manifest.validate().unwrap();
        assert_eq!(manifest.api, 3);
        assert_eq!(manifest.host_abi, 3);
        assert!(manifest.panel("main"));
        assert_eq!(manifest.full_command_id("greet"), "plugin.dev.tervo.echo.greet");
        assert_eq!(
            manifest.permission_fingerprint(),
            vec![
                "filesystem=plugin".to_string(),
                "clipboard".to_string(),
                "terminal_context".to_string(),
                "credentials=own".to_string()
            ]
        );
    }

    #[test]
    fn api_v2_is_rejected_with_migration_reason() {
        let text = GOOD.replace("api = 3", "api = 2");
        let manifest = parse(&text);
        let err = manifest.validate().unwrap_err();
        assert!(err.contains("v1/v2"), "{err}");
    }

    #[test]
    fn host_abi_mismatch_is_rejected() {
        let text = GOOD.replace("host_abi = 3", "host_abi = 2");
        let manifest = parse(&text);
        assert!(manifest.validate().unwrap_err().contains("host_abi"));
    }

    #[test]
    fn oneshot_runtime_is_rejected() {
        let text = GOOD.replace("runtime = \"process\"", "runtime = \"oneshot\"");
        let manifest = parse(&text);
        assert!(manifest.validate().unwrap_err().contains("oneshot"));
    }

    #[test]
    fn unknown_manifest_field_is_rejected() {
        let text = format!("{GOOD}\nwasm = true\n");
        assert!(toml::from_str::<Manifest>(&text).is_err());
    }

    #[test]
    fn unknown_permission_value_is_rejected() {
        let text = GOOD.replace("filesystem = \"plugin\"", "filesystem = \"everywhere\"");
        let manifest = parse(&text);
        assert!(manifest.validate().unwrap_err().contains("filesystem"));
    }

    #[test]
    fn commands_require_contribution_flag() {
        let text = GOOD.replace("commands = true", "commands = false");
        let manifest = parse(&text);
        assert!(manifest.validate().unwrap_err().contains("commands"));
    }

    #[test]
    fn unknown_event_is_rejected() {
        let text = GOOD.replace("\"pane-focus\"", "\"pane-blur\"");
        let manifest = parse(&text);
        assert!(manifest.validate().unwrap_err().contains("未知事件"));
    }

    #[test]
    fn assistant_accepts_bool_and_table_but_is_placeholder() {
        let text = GOOD.replace("assistant = false", "assistant = { name = \"Echo Assist\" }");
        let manifest = parse(&text);
        manifest.validate().unwrap();
        assert!(manifest.contributes.assistant_enabled());
        let text = GOOD.replace("assistant = false", "assistant = true");
        let manifest = parse(&text);
        manifest.validate().unwrap();
        assert!(manifest.contributes.assistant_enabled());
    }

    #[test]
    fn path_escape_is_rejected() {
        assert!(validate_relative_path("../evil").is_err());
        assert!(validate_relative_path("/usr/bin/sh").is_err());
        assert!(validate_relative_path("bin/../../evil").is_err());
        assert!(validate_relative_path("run.sh").is_ok());
        assert!(validate_relative_path("bin/run.sh").is_ok());
    }

    #[test]
    fn id_rejects_unsafe_characters() {
        assert!(validate_id("dev.tervo.echo-1", "id").is_ok());
        assert!(validate_id("1plugin", "id").is_ok());
        assert!(validate_id("../evil", "id").is_err());
        assert!(validate_id("bad id", "id").is_err());
        assert!(validate_id("", "id").is_err());
    }

    #[test]
    fn event_roundtrip() {
        for event in PluginEvent::ALL {
            assert_eq!(PluginEvent::parse(event.as_str()), Some(event));
        }
        assert_eq!(PluginEvent::parse("nope"), None);
    }
}
