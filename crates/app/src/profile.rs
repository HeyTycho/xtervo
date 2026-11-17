// SPDX-License-Identifier: GPL-3.0-only
//! 连接 Profile：名字 + 启动方式（当前只做 Local，SSH/Serial/Telnet 后续扩展）。
//!
//! 与 `config.rs` 同款策略：版本化、原子写、解析失败不覆盖用户文件。
//! 存到 `~/.config/tervo/profiles.toml`（`TERVO_PROFILES` 可覆盖，便于测试/多实例）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tervo_core::CursorShape;

/// 当前 Profile 结构版本（v2 引入 kind 与连接字段）。
///
/// v2 的外观字段（font_size/theme/cursor_*/scrollback）是 **additive** 的：
/// 旧版程序读到新文件只会忽略不认识的键，因此不 bump 版本（bump 会让旧版
/// 把整个 profiles.toml 当成“未来版本”丢弃）。旧文件缺这些键时 serde
/// default 补 `None`（= 继承全局配置），v1 迁移路径保持不变。
pub const VERSION: u32 = 2;

/// Profile 允许的字号范围，与设置页/配置的 8–40 保持一致。
pub const MIN_FONT_SIZE: f32 = 8.0;
/// 见 [`MIN_FONT_SIZE`]。
pub const MAX_FONT_SIZE: f32 = 40.0;

/// 一个本地 Profile：决定新 Tab 用什么 shell / 参数 / 起始目录 / 环境变量。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// 稳定 ID（文件内唯一；删除后不复用）。
    pub id: u64,
    pub name: String,
    /// 自定义 Tab 标题（LOCAL-003）；`None` = 用 `name`。
    ///
    /// 只影响新开 Tab 的标题，不影响 Profile 的管理/默认项识别；会话快照
    /// 保存的是 Tab 标题本身，因此恢复后标题保持用户看到的样子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 连接意外结束时是否自动重连（仅 ssh/telnet/serial 有意义；默认关）。
    #[serde(default)]
    pub auto_reconnect: bool,
    /// 绑定到该 Profile 的快捷键（单段，形如 `ctrl+alt+1`）；空 = 不绑定。
    /// 必须带 ctrl/alt 之一，避免吞掉普通输入；非法值在 `validate` 里拒绝。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hotkey: String,
    /// `local` | `ssh` | `serial` | `telnet`（v1 没有该字段，迁移时补 local）。
    pub kind: String,
    // ---- local ----
    /// 空 = 用系统默认 shell。
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    // ---- ssh / telnet ----
    pub host: String,
    pub port: u16,
    pub user: String,
    /// `agent` | `key` | `password`（密码走 Vault，绝不写 profiles.toml）。
    pub auth: String,
    pub key_path: String,
    /// `auth=password` 时是否把密码保存到 Vault（默认 false；明文密码永不落 profiles.toml）。
    pub save_password: bool,
    /// 传给 ssh 的追加参数（如 -o UserKnownHostsFile=... / -o StrictHostKeyChecking=yes）。
    pub extra_args: Vec<String>,
    /// 本地端口转发，每项形如 `8080:localhost:80` → `ssh -L 8080:localhost:80`。
    pub local_forwards: Vec<String>,
    /// 远程端口转发，每项形如 `8080:localhost:80` → `ssh -R 8080:localhost:80`。
    pub remote_forwards: Vec<String>,
    /// 动态转发（SOCKS），每项形如 `1080` / `127.0.0.1:1080` → `ssh -D ...`。
    pub dynamic_forwards: Vec<String>,
    /// ProxyJump（跳板机），原样传给 `ssh -J`（如 `user@jump:2222`）；空 = 直连。
    ///
    /// 只存跳板串，不存密码；跳板机的认证交给系统 ssh（agent/key/交互均可）。
    pub proxy_jump: String,
    /// 是否请求 ssh agent forwarding（`ssh -A`）。
    pub agent_forward: bool,
    // ---- serial ----
    pub serial_port: String,
    pub baud: u32,
    // ---- 外观（None / 空 = 继承全局 Config；对所有 kind 生效）----
    /// 该 Profile 打开的 pane 使用的字号；`None` = 继承 `config.font_size`。
    pub font_size: Option<f32>,
    /// 主题 id（`auto` / 内置主题 id / `custom:<stem>`）；`None` = 继承
    /// `config.theme`。未知 id 在打开时回退当前全局主题（不改写 Profile）。
    pub theme: Option<String>,
    /// `block` | `underline` | `ibeam`；`None` = 继承 `config.cursor_shape`。
    pub cursor_shape: Option<String>,
    /// `on` | `off` | `system`；`None` = 继承 `config.cursor_blink`。
    pub cursor_blink: Option<String>,
    /// 历史缓冲行数；`None` = 继承 `config.scrollback` / `scrollback_unlimited`。
    pub scrollback: Option<usize>,
}

/// 校验一条本地转发规格：`本地端口:主机:端口`，端口 1–65535。
pub fn validate_forward(spec: &str) -> Result<(), String> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 3 || parts.iter().any(|x| x.trim().is_empty()) {
        return Err(format!("端口转发格式应为 本地端口:主机:端口，实际: {spec}"));
    }
    if parts[0].trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return Err(format!("端口转发的本地端口无效: {spec}"));
    }
    if parts[parts.len() - 1].trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return Err(format!("端口转发的目标端口无效: {spec}"));
    }
    Ok(())
}

/// 支持的 Profile 类型（字符串值即配置里的 `kind`）。
pub const KINDS: [&str; 4] = ["local", "ssh", "serial", "telnet"];

/// 校验远程转发规格：`[绑定地址:]端口:主机:端口`，端口 1–65535。
///
/// 与本地转发的区别是允许 OpenSSH 的可选绑定地址（4 段）；
/// 只做结构校验，主机名解析/权限错误交给 ssh 报。
pub fn validate_remote_forward(spec: &str) -> Result<(), String> {
    let parts: Vec<&str> = spec.split(':').collect();
    if !(parts.len() == 3 || parts.len() == 4) || parts.iter().any(|x| x.trim().is_empty()) {
        return Err(format!("远程转发格式应为 [绑定地址:]端口:主机:端口，实际: {spec}"));
    }
    let port_index = if parts.len() == 4 { 1 } else { 0 };
    let hostport_index = parts.len() - 1;
    if parts[port_index].trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return Err(format!("远程转发的本地端口无效: {spec}"));
    }
    if parts[hostport_index].trim().parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return Err(format!("远程转发的目标端口无效: {spec}"));
    }
    Ok(())
}

/// 校验动态转发（SOCKS）规格：`[绑定地址:]端口`。
pub fn validate_dynamic_forward(spec: &str) -> Result<(), String> {
    let parts: Vec<&str> = spec.split(':').collect();
    if !(parts.len() == 1 || parts.len() == 2) || parts.iter().any(|x| x.trim().is_empty()) {
        return Err(format!("动态转发格式应为 [绑定地址:]端口，实际: {spec}"));
    }
    let port = parts[parts.len() - 1].trim();
    if port.parse::<u16>().map(|p| p == 0).unwrap_or(true) {
        return Err(format!("动态转发端口无效: {spec}"));
    }
    Ok(())
}

/// 校验 ProxyJump 串：非空、无空白、不能以 `-` 开头（防止被当成 ssh 选项）。
///
/// 用户/主机/端口的细节（`user@host:port`、IPv6、逗号多跳）交给 ssh 自己解析；
/// 这里只拦住一定会破坏命令行的输入。
pub fn validate_proxy_jump(spec: &str) -> Result<(), String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(());
    }
    if spec.chars().any(char::is_whitespace) {
        return Err(format!("ProxyJump 不能包含空白: {spec}"));
    }
    if spec.starts_with('-') {
        return Err(format!("ProxyJump 不能以 - 开头: {spec}"));
    }
    Ok(())
}


/// 校验“起始目录”是否真的可用（LOCAL-003 表单测试按钮）。
///
/// 只做本地文件系统检查：展开 `~`、确认是目录、返回规范化路径。空串表示
/// 继承 shell 默认目录，但测试按钮需要用户明确输入才有意义，故空串报错。
/// 不做 canonicalize：符号链接指向的目录也是用户会 cd 进去的目录。
pub fn test_start_dir(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("起始目录为空（留空 = 用 shell 默认目录）".into());
    }
    let path = crate::config::expand_tilde(raw);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => Ok(path),
        Ok(_) => Err(format!("{} 不是目录", path.display())),
        Err(e) => Err(format!("无法访问 {}: {e}", path.display())),
    }
}

/// 光标形状是否是可识别的别名（与 [`CursorShape::from_name`] 的接受集合同步）。
fn known_cursor_shape(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str(),
        "block" | "underline" | "under-line" | "ibeam" | "i-beam" | "beam" | "bar"
            | "hollow-block" | "hollow" | "hidden" | "none"
    )
}

pub fn default_kind() -> String {
    "local".to_string()
}

/// 按搜索框输入过滤 Profile，返回命中项在 store 里的原始下标（保持列表顺序）。
///
/// 规则：空查询（含全空白）返回全部；否则对 `name` / `host` / `kind` 做
/// **大小写不敏感**的子串匹配。返回下标而不是克隆，面板点击时仍能定位到
/// 唯一的 Profile（即使过滤后列表顺序/数量变化）。
pub fn filter_indices(profiles: &[Profile], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..profiles.len()).collect();
    }
    profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.name.to_lowercase().contains(&needle)
                || p.host.to_lowercase().contains(&needle)
                || p.kind.to_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 复制 Profile 时生成不冲突的名字：`原名 (copy 1)`、`原名 (copy 2)`……
pub fn duplicate_name(base: &str, profiles: &[Profile]) -> String {
    let base = base.trim();
    let mut n = 1u64;
    loop {
        let candidate = format!("{base} (copy {n})");
        if !profiles.iter().any(|p| p.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            title: None,
            auto_reconnect: false,
            hotkey: String::new(),
            kind: default_kind(),
            shell: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: "agent".to_string(),
            key_path: String::new(),
            save_password: false,
            extra_args: Vec::new(),
            local_forwards: Vec::new(),
            remote_forwards: Vec::new(),
            dynamic_forwards: Vec::new(),
            proxy_jump: String::new(),
            agent_forward: false,
            serial_port: String::new(),
            baud: 115200,
            font_size: None,
            theme: None,
            cursor_shape: None,
            cursor_blink: None,
            scrollback: None,
        }
    }
}

impl Profile {
    /// 归一化外观字段：去空白、空串 → `None`（继承全局）、字号夹取到 8–40。
    ///
    /// 校验失败的原始值（例如 `font_size = 0`）会在 [`Profile::validate`] 里被
    /// 拒绝；这里只做“读盘后恢复到可运行状态”的兜底，不静默改写用户文件。
    pub fn normalize_appearance(&mut self) {
        self.title = self
            .title
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.font_size = self
            .font_size
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| {
                let clamped = v.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
                (clamped * 10.0).round() / 10.0
            });
        self.theme = self
            .theme
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.cursor_shape = self
            .cursor_shape
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.cursor_blink = self
            .cursor_blink
            .take()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        self.scrollback = self.scrollback.filter(|n| *n > 0);
    }

    /// 新开 Tab 使用的标题：自定义标题优先，否则 Profile 名。
    pub fn effective_title(&self) -> &str {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.name.as_str())
    }

    /// 有效字号；`None` = 继承全局 `config.font_size`。
    pub fn effective_font_size(&self) -> Option<f32> {
        self.font_size
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE))
    }

    /// 有效主题 id；空串/空白视为继承全局。
    pub fn effective_theme(&self) -> Option<&str> {
        self.theme.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// 有效光标形状；未知别名按 [`CursorShape::from_name`] 回退 block。
    pub fn effective_cursor_shape(&self) -> Option<CursorShape> {
        self.effective_cursor_shape_name()
            .map(CursorShape::from_name)
    }

    /// 规范化后的光标形状名（供 UI/测试展示）。
    pub fn effective_cursor_shape_name(&self) -> Option<&str> {
        self.cursor_shape
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// 有效光标闪烁：`Some(true)` 闪、`Some(false)` 不闪、`None` 继承全局。
    /// `system` 表示交给全局配置（等同 `None`）。
    pub fn effective_cursor_blink(&self) -> Option<bool> {
        match self.cursor_blink.as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("on" | "true" | "yes" | "1") => Some(true),
            Some("off" | "false" | "no" | "0") => Some(false),
            _ => None,
        }
    }

    /// 有效 scrollback 行数；`None` = 继承全局（含 unlimited 语义）。
    pub fn effective_scrollback(&self) -> Option<usize> {
        self.scrollback.filter(|n| *n > 0)
    }

    /// SSH 启动命令：用系统 `ssh` 客户端跑在 PTY 里（原生 russh 引擎后续再做）。
    ///
    /// 参数顺序：`[-i key] [-o password-only...] [-A] [-J jump] -p port [-l user]
    /// [-L ...] [-R ...] [-D ...] [extra] host`，与 OpenSSH 用法一致。
    pub fn ssh_command(&self) -> (String, Vec<String>) {
        let mut args = Vec::new();
        if self.auth == "key" && !self.key_path.trim().is_empty() {
            args.push("-i".to_string());
            args.push(crate::config::expand_tilde(self.key_path.trim()).display().to_string());
        }
        if self.auth == "password" {
            // auth=password 的语义是「只用密码」：不能让 agent/密钥先认证成功，否则
            // Vault 密码与 askpass 永远不会被触发（服务器接受任意公钥时表现为
            // 服务端看不到 password auth、askpass 从未被调用）。
            // keyboard-interactive 放在 password 之后，兼容只提供 PAM kbd-int 的服务器；
            // KbdInteractiveAuthentication 显式打开，避免用户 ssh_config 里关掉后
            // 只提供 kbd-int 的服务器直接认证失败。
            args.push("-o".to_string());
            args.push("PubkeyAuthentication=no".to_string());
            args.push("-o".to_string());
            args.push("PreferredAuthentications=password,keyboard-interactive".to_string());
            args.push("-o".to_string());
            args.push("KbdInteractiveAuthentication=yes".to_string());
        }
        if self.agent_forward {
            args.push("-A".to_string());
        }
        if !self.proxy_jump.trim().is_empty() {
            args.push("-J".to_string());
            args.push(self.proxy_jump.trim().to_string());
        }
        args.push("-p".to_string());
        args.push(self.port.max(1).to_string());
        if !self.user.trim().is_empty() {
            args.push("-l".to_string());
            args.push(self.user.trim().to_string());
        }
        for f in &self.local_forwards {
            args.push("-L".to_string());
            args.push(f.clone());
        }
        for f in &self.remote_forwards {
            args.push("-R".to_string());
            args.push(f.clone());
        }
        for f in &self.dynamic_forwards {
            args.push("-D".to_string());
            args.push(f.clone());
        }
        args.extend(self.extra_args.iter().filter(|a| !a.trim().is_empty()).cloned());
        args.push(self.host.trim().to_string());
        ("ssh".to_string(), args)
    }

    /// Telnet 启动命令（系统 telnet 客户端）。
    pub fn telnet_command(&self) -> (String, Vec<String>) {
        ("telnet".to_string(), vec![self.host.trim().to_string(), self.port.max(1).to_string()])
    }

    /// Serial 启动命令（socat 把 PTY 和串口设备桥起来；没有 socat 时启动会失败并提示）。
    pub fn serial_command(&self) -> (String, Vec<String>) {
        let dev = self.serial_port.trim();
        let baud = self.baud.max(1);
        (
            "socat".to_string(),
            vec!["-,raw,echo=0".to_string(), format!("{dev},raw,echo=0,b{baud}")],
        )
    }

    /// 按类型校验必填项（本地 Profile 几乎不校验，SSH/Serial 必须有目标）。
    /// 第 `attempt` 次自动重连的等待时间；超过 `MAX_RECONNECT_ATTEMPTS` 返回
    /// `None`（停止重连）。纯函数，便于单测；调用方负责真正调度定时器。
    pub fn reconnect_delay(attempt: u32) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(match attempt {
            0 | 1 => 2,
            2 => 5,
            3 => 10,
            _ => return None,
        }))
    }

    /// 解析绑定的快捷键；空串或非法值返回 `None`（非法值另由 `validate` 拒绝）。
    ///
    /// 只接受单段 chord：Profile 热键要能一次按键直接开 Tab，不做 multi-chord。
    pub fn hotkey_chord(&self) -> Option<crate::commands::Chord> {
        let text = self.hotkey.trim();
        if text.is_empty() {
            return None;
        }
        crate::commands::Chord::parse(text).ok()
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.hotkey.trim().is_empty() {
            let chord = self
                .hotkey_chord()
                .ok_or_else(|| format!("非法快捷键: {}", self.hotkey))?;
            if !(chord.mods.ctrl || chord.mods.alt) {
                return Err(format!(
                    "快捷键必须包含 Ctrl 或 Alt，避免吞掉普通输入: {}",
                    self.hotkey
                ));
            }
        }
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(format!("未知 Profile 类型: {}", self.kind));
        }
        // 外观是可选项，但显式写下的值必须可解释；空串/None = 继承全局。
        if let Some(size) = self.font_size {
            if !size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
                return Err(format!(
                    "字号必须在 {MIN_FONT_SIZE:.0}–{MAX_FONT_SIZE:.0} 之间，实际: {size}"
                ));
            }
        }
        if let Some(shape) = &self.cursor_shape {
            if !shape.trim().is_empty() && !known_cursor_shape(shape) {
                return Err(format!("未知光标形状: {shape}（block/underline/ibeam）"));
            }
        }
        if let Some(blink) = &self.cursor_blink {
            let key = blink.trim().to_ascii_lowercase();
            if !key.is_empty() && !matches!(key.as_str(), "on" | "off" | "system") {
                return Err(format!("未知光标闪烁值: {blink}（on/off/system）"));
            }
        }
        if self.scrollback == Some(0) {
            return Err("Profile scrollback 必须大于 0（留空 = 继承全局）".into());
        }
        match self.kind.as_str() {
            "ssh" => {
                if self.host.trim().is_empty() {
                    return Err("SSH 主机不能为空".into());
                }
                if self.port == 0 {
                    return Err("SSH 端口必须在 1-65535".into());
                }
                if self.user.trim().is_empty() {
                    return Err("SSH 用户名不能为空".into());
                }
                if !["agent", "key", "password"].contains(&self.auth.as_str()) {
                    return Err(format!("未知认证方式: {}", self.auth));
                }
                if self.auth == "key" && self.key_path.trim().is_empty() {
                    return Err("选择 key 认证时必须填私钥路径".into());
                }
                for f in &self.local_forwards {
                    validate_forward(f)?;
                }
                for f in &self.remote_forwards {
                    validate_remote_forward(f)?;
                }
                for f in &self.dynamic_forwards {
                    validate_dynamic_forward(f)?;
                }
                validate_proxy_jump(&self.proxy_jump)?;
            },
            "serial" => {
                if self.serial_port.trim().is_empty() {
                    return Err("串口设备不能为空".into());
                }
                if self.baud == 0 {
                    return Err("波特率必须大于 0".into());
                }
            },
            "telnet" => {
                if self.host.trim().is_empty() {
                    return Err("Telnet 主机不能为空".into());
                }
                if self.port == 0 {
                    return Err("Telnet 端口必须在 1-65535".into());
                }
            },
            _ => {},
        }
        Ok(())
    }
}

/// Profile 集合。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: Vec<Profile>,
    /// 下一个可用 ID（持久化，避免重启后复用已删除的 ID）。
    pub next_id: u64,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self { version: VERSION, profiles: Vec::new(), next_id: 1 }
    }
}

/// `profiles.toml` + v1 → `profiles.toml.v1.bak`。
fn backup_path(path: &Path, from: u32) -> PathBuf {
    let name = path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default();
    path.with_file_name(format!("{name}.v{from}.bak"))
}

impl ProfileStore {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("TERVO_PROFILES") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
            });
        base.join("tervo").join("profiles.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut store: ProfileStore = match toml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[profile] {} 解析失败，本次用空列表（原文件未改）: {e}", path.display());
                return Self::default();
            },
        };
        if store.version > VERSION {
            eprintln!("[profile] 文件版本 v{} 比程序新（v{VERSION}），忽略", store.version);
            return Self::default();
        }
        if store.version < VERSION {
            let from = store.version;
            // 与 config.rs 一致：迁移前留一份原文件，回滚/排障时可直接恢复。
            let backup = backup_path(path, from);
            match std::fs::copy(path, &backup) {
                Ok(_) => eprintln!(
                    "[profile] 迁移 v{from} → v{VERSION} 前已备份 {}",
                    backup.display()
                ),
                Err(e) => eprintln!("[profile] 备份失败（继续迁移）: {e}"),
            }
            // v1 没有 kind 字段：serde default 已补成 local，这里补版本并写回
            for p in &mut store.profiles {
                if !KINDS.contains(&p.kind.as_str()) {
                    p.kind = default_kind();
                }
                p.normalize_appearance();
            }
            store.version = VERSION;
            store.fix_ids();
            store.save_to(path);
            eprintln!("[profile] 已迁移到 v{VERSION}");
            return store;
        }
        // 外观字段是 additive 的：旧 v2 文件缺字段 -> None（继承全局）；
        // 手写文件里的空串/越界字号在内存里归一，不改写原文件。
        for p in &mut store.profiles {
            p.normalize_appearance();
        }
        store.fix_ids();
        store
    }

    pub fn save(&self) {
        self.save_to(&Self::path());
    }

    pub fn save_to(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let text = match toml::to_string_pretty(self) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[profile] 序列化失败: {e}");
                return;
            },
        };
        let tmp = path.with_extension("toml.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            eprintln!("[profile] 写入 {} 失败: {e}", tmp.display());
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            eprintln!("[profile] 替换 {} 失败: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 新增 Profile（同名或空名拒绝），返回新 ID。
    pub fn add(&mut self, mut profile: Profile) -> Result<u64, String> {
        let name = profile.name.trim().to_string();
        if name.is_empty() {
            return Err("名字不能为空".into());
        }
        if self.profiles.iter().any(|p| p.name == name) {
            return Err(format!("已存在同名 Profile: {name}"));
        }
        profile.validate()?;
        profile.normalize_appearance();
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        profile.id = id;
        profile.name = name;
        self.profiles.push(profile);
        Ok(id)
    }

    /// 更新已有 Profile（保留 ID；名字不能与其他 Profile 冲突）。
    pub fn update(&mut self, id: u64, mut updated: Profile) -> Result<(), String> {
        let name = updated.name.trim().to_string();
        if name.is_empty() {
            return Err("名字不能为空".into());
        }
        if self.profiles.iter().any(|p| p.id != id && p.name == name) {
            return Err(format!("已存在同名 Profile: {name}"));
        }
        updated.id = id;
        updated.name = name;
        updated.validate()?;
        updated.normalize_appearance();
        let slot = self.profiles.iter_mut().find(|p| p.id == id).ok_or("Profile 不存在")?;
        *slot = updated;
        Ok(())
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        self.profiles.len() != before
    }

    pub fn get(&self, id: u64) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// 解析默认 Profile 的 store 下标：`id == 0`（未设置）或 ID 已失效时返回 `None`，
    /// 调用方据此回退到系统默认 shell（绝不因为坏配置拒绝开新 Tab）。
    pub fn default_index(&self, id: u64) -> Option<usize> {
        if id == 0 {
            return None;
        }
        self.profiles.iter().position(|p| p.id == id)
    }

    /// 复制一条 Profile：字段全量克隆、名字按 `duplicate_name` 去重、ID 重新分配。
    /// 调用方负责 `save()` 落盘。
    pub fn duplicate(&mut self, id: u64) -> Result<u64, String> {
        let mut copy = self.get(id).cloned().ok_or("Profile 不存在")?;
        copy.name = duplicate_name(&copy.name, &self.profiles);
        self.add(copy)
    }

    /// 把 ID 修正成唯一且 < next_id（手改文件后也不至于开新 Tab 全撞一个 ID）。
    fn fix_ids(&mut self) {
        let mut seen = std::collections::HashSet::new();
        let mut max_id = 0;
        for p in &mut self.profiles {
            while p.id == 0 || !seen.insert(p.id) {
                p.id = max_id + 1;
                max_id = p.id;
            }
            max_id = max_id.max(p.id);
        }
        self.next_id = self.next_id.max(max_id + 1).max(1);
        self.version = VERSION;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_is_bounded_and_monotonic() {
        use std::time::Duration;
        assert_eq!(Profile::reconnect_delay(1), Some(Duration::from_secs(2)));
        assert_eq!(Profile::reconnect_delay(2), Some(Duration::from_secs(5)));
        assert_eq!(Profile::reconnect_delay(3), Some(Duration::from_secs(10)));
        assert_eq!(Profile::reconnect_delay(4), None, "第 4 次应停止重连");
        assert!(Profile::reconnect_delay(1) < Profile::reconnect_delay(2));
        assert!(Profile::reconnect_delay(2) < Profile::reconnect_delay(3));
        // 默认关，且磁盘往返保留
        let p = Profile::default();
        assert!(!p.auto_reconnect);
        let mut on = Profile::default();
        on.auto_reconnect = true;
        let dir = tmpdir("reconnect");
        let path = dir.join("profiles.toml");
        let mut store = ProfileStore::default();
        on.name = "prod".into();
        store.add(on).unwrap();
        store.save_to(&path);
        assert!(ProfileStore::load_from(&path).profiles[0].auto_reconnect);
    }

    #[test]
    fn add_rejects_hotkey_without_modifier_via_validate() {
        // 表单保存走 ProfileStore::add/update -> profile.validate()，这里锁住这条链路。
        let mut store = ProfileStore::default();
        let mut p = Profile::default();
        p.name = "risky".into();
        p.hotkey = "shift+x".into();
        let err = store.add(p).unwrap_err();
        assert!(err.contains("Ctrl 或 Alt"), "{err}");
        assert!(store.profiles.is_empty(), "校验失败不能写入 store");

        let mut ok = Profile::default();
        ok.name = "fine".into();
        ok.hotkey = "ctrl+alt+k".into();
        let id = store.add(ok).expect("合法热键应能保存");
        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].hotkey, "ctrl+alt+k");
        assert!(id >= 1);
    }

    #[test]
    fn profile_hotkey_parses_roundtrips_and_rejects_risky_bindings() {
        let mut p = Profile::default();
        assert!(p.hotkey_chord().is_none(), "空绑定 = 无热键");

        p.hotkey = "ctrl+alt+1".into();
        let chord = p.hotkey_chord().expect("应能解析");
        assert!(chord.mods.ctrl && chord.mods.alt);
        assert_eq!(chord.key, "1");
        p.validate().expect("带 Ctrl+Alt 的绑定合法");

        // 非法组合键
        p.hotkey = "ctrl+nonsense".into();
        assert!(p.hotkey_chord().is_none());
        assert!(p.validate().unwrap_err().contains("非法快捷键"));

        // 没有 Ctrl/Alt 会吞普通输入
        p.hotkey = "shift+x".into();
        assert!(p.validate().unwrap_err().contains("Ctrl 或 Alt"));

        // 磁盘往返保留 hotkey
        p.hotkey = "ctrl+alt+p".into();
        let dir = tmpdir("hotkey");
        let path = dir.join("profiles.toml");
        let mut store = ProfileStore::default();
        store.profiles.push(p.clone());
        store.next_id = 2;
        store.save_to(&path);
        let back = ProfileStore::load_from(&path);
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].hotkey, "ctrl+alt+p");
        assert_eq!(back.profiles[0].hotkey_chord(), p.hotkey_chord());
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rt-prof-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(name: &str) -> Profile {
        Profile { name: name.into(), shell: "/bin/bash".into(), ..Profile::default() }
    }

    #[test]
    fn add_enforces_unique_nonempty_names_and_ids() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("work")).unwrap();
        let b = s.add(sample("home")).unwrap();
        assert_ne!(a, b);
        assert!(s.add(sample("work")).is_err(), "同名要拒绝");
        assert!(s.add(sample("   ")).is_err(), "空名要拒绝");
        assert_eq!(s.profiles.len(), 2);
    }

    #[test]
    fn roundtrip_and_atomic_save() {
        let dir = tmpdir("roundtrip");
        let path = dir.join("profiles.toml");
        let mut s = ProfileStore::default();
        s.add(Profile {
            name: "dev".into(),
            shell: "/bin/zsh".into(),
            args: vec!["-l".into()],
            cwd: Some("/home/user/work".into()),
            env: vec![("FOO".into(), "1".into())],
            ..Profile::default()
        })
        .unwrap();
        s.save_to(&path);
        assert!(!dir.join("profiles.toml.tmp").exists());

        let back = ProfileStore::load_from(&path);
        assert_eq!(back.profiles.len(), 1);
        assert_eq!(back.profiles[0].shell, "/bin/zsh");
        assert_eq!(back.profiles[0].args, vec!["-l"]);
        assert_eq!(back.profiles[0].cwd.as_deref(), Some("/home/user/work"));
        assert_eq!(back.profiles[0].env, vec![("FOO".to_string(), "1".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appearance_fields_roundtrip_and_missing_inherits() {
        let dir = tmpdir("appearance");
        let path = dir.join("profiles.toml");
        let mut s = ProfileStore::default();
        s.add(Profile {
            name: "pretty".into(),
            font_size: Some(18.0),
            theme: Some("catppuccin-mocha".into()),
            cursor_shape: Some("underline".into()),
            cursor_blink: Some("off".into()),
            scrollback: Some(12_000),
            ..Profile::default()
        })
        .unwrap();
        s.add(Profile { name: "inherit".into(), ..Profile::default() })
            .unwrap();
        s.save_to(&path);

        let back = ProfileStore::load_from(&path);
        let pretty = back.profiles.iter().find(|p| p.name == "pretty").unwrap();
        assert_eq!(pretty.effective_font_size(), Some(18.0));
        assert_eq!(pretty.effective_theme(), Some("catppuccin-mocha"));
        assert_eq!(pretty.effective_cursor_shape_name(), Some("underline"));
        assert_eq!(pretty.effective_cursor_blink(), Some(false));
        assert_eq!(pretty.effective_scrollback(), Some(12_000));

        let inherit = back.profiles.iter().find(|p| p.name == "inherit").unwrap();
        assert_eq!(inherit.effective_font_size(), None);
        assert_eq!(inherit.effective_theme(), None);
        assert_eq!(inherit.effective_cursor_shape(), None);
        assert_eq!(inherit.effective_cursor_blink(), None);
        assert_eq!(inherit.effective_scrollback(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_without_appearance_fields_loads_as_inherit_without_rewrite() {
        let dir = tmpdir("v2-additive");
        let path = dir.join("profiles.toml");
        let original = r#"
version = 2
next_id = 2

[[profiles]]
id = 1
name = "old"
kind = "local"
shell = "/bin/sh"
"#;
        std::fs::write(&path, original).unwrap();
        let store = ProfileStore::load_from(&path);
        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].font_size, None);
        assert_eq!(store.profiles[0].theme, None);
        assert_eq!(store.profiles[0].cursor_shape, None);
        assert_eq!(store.profiles[0].cursor_blink, None);
        assert_eq!(store.profiles[0].scrollback, None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "v2 additive 字段缺失时不应因读盘而改写用户文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_written_appearance_is_normalized_on_load() {
        let dir = tmpdir("appearance-normalize");
        let path = dir.join("profiles.toml");
        std::fs::write(
            &path,
            r#"
version = 2
next_id = 2

[[profiles]]
id = 1
name = "odd"
kind = "local"
font_size = 99.0
theme = "  "
cursor_shape = "  UNDERLINE "
cursor_blink = " OFF "
scrollback = 0
"#,
        )
        .unwrap();
        let store = ProfileStore::load_from(&path);
        let p = &store.profiles[0];
        assert_eq!(p.font_size, Some(40.0), "读盘时夹取到安全上限");
        assert_eq!(p.theme, None, "空白主题 = 继承全局");
        assert_eq!(p.cursor_shape.as_deref(), Some("UNDERLINE"), "trim 保留原始大小写");
        assert_eq!(p.cursor_blink.as_deref(), Some("off"));
        assert_eq!(p.scrollback, None, "0 行视为继承全局");
        assert_eq!(p.effective_cursor_shape_name(), Some("UNDERLINE"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appearance_validation_rejects_uninterpretable_values() {
        let base = sample("a");
        for bad in [4.0, 100.0, f32::NAN, 0.0] {
            let p = Profile { font_size: Some(bad), ..base.clone() };
            assert!(p.validate().is_err(), "font_size={bad} 必须拒绝");
        }
        let p = Profile { cursor_shape: Some("diamond".into()), ..base.clone() };
        assert!(p.validate().unwrap_err().contains("光标形状"));
        let p = Profile { cursor_blink: Some("sometimes".into()), ..base.clone() };
        assert!(p.validate().unwrap_err().contains("闪烁"));
        let p = Profile { scrollback: Some(0), ..base.clone() };
        assert!(p.validate().unwrap_err().contains("scrollback"));
        // 合法边界/别名可通过
        for (size, shape, blink) in [
            (MIN_FONT_SIZE, "block", "on"),
            (MAX_FONT_SIZE, "ibeam", "off"),
            (15.0, "bar", "system"),
            (15.0, "hollow-block", "ON"),
        ] {
            let p = Profile {
                font_size: Some(size),
                cursor_shape: Some(shape.into()),
                cursor_blink: Some(blink.into()),
                ..base.clone()
            };
            assert!(p.validate().is_ok(), "{size} {shape} {blink} 应通过");
        }
        // 空白 = 继承，不算非法值
        let p = Profile {
            cursor_shape: Some("  ".into()),
            cursor_blink: Some("".into()),
            ..base
        };
        assert!(p.validate().is_ok());
    }

    #[test]
    fn appearance_helpers_cover_aliases_and_inherit_semantics() {
        let p = Profile {
            cursor_blink: Some("system".into()),
            cursor_shape: Some("a custom shape".into()),
            theme: Some(" auto ".into()),
            ..Profile::default()
        };
        assert_eq!(p.effective_cursor_blink(), None, "system 表示交给全局");
        assert_eq!(p.effective_cursor_shape(), Some(CursorShape::Block));
        assert_eq!(p.effective_theme(), Some("auto"));
        let p = Profile { cursor_shape: Some("beam".into()), ..Profile::default() };
        assert_eq!(p.effective_cursor_shape(), Some(CursorShape::Beam));
        let p = Profile { cursor_blink: Some(" true ".into()), ..Profile::default() };
        assert_eq!(p.effective_cursor_blink(), Some(true));
    }

    #[test]
    fn add_and_update_normalize_optional_appearance() {
        let mut s = ProfileStore::default();
        let id = s
            .add(Profile {
                name: "n".into(),
                theme: Some("  yaru-dark  ".into()),
                cursor_blink: Some(" OFF ".into()),
                ..Profile::default()
            })
            .unwrap();
        let p = s.get(id).unwrap();
        assert_eq!(p.theme.as_deref(), Some("yaru-dark"));
        assert_eq!(p.cursor_blink.as_deref(), Some("off"));
        let mut edited = p.clone();
        edited.theme = Some("   ".into());
        edited.font_size = Some(20.04);
        s.update(id, edited).unwrap();
        let p = s.get(id).unwrap();
        assert_eq!(p.theme, None);
        assert_eq!(p.font_size, Some(20.0), "字号保留一位小数");
    }

    #[test]
    fn broken_file_keeps_original() {
        let dir = tmpdir("broken");
        let path = dir.join("profiles.toml");
        let broken = "profiles = [oops\n";
        std::fs::write(&path, broken).unwrap();
        let s = ProfileStore::load_from(&path);
        assert!(s.profiles.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_are_repaired_on_load() {
        let dir = tmpdir("ids");
        let path = dir.join("profiles.toml");
        std::fs::write(
            &path,
            r#"
version = 1
next_id = 1

[[profiles]]
id = 7
name = "a"
shell = "/bin/sh"

[[profiles]]
id = 7
name = "b"
shell = "/bin/sh"
"#,
        )
        .unwrap();
        let s = ProfileStore::load_from(&path);
        assert_eq!(s.profiles.len(), 2);
        let ids: std::collections::HashSet<u64> = s.profiles.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 2, "重复 ID 应被修正");
        assert!(s.next_id > *ids.iter().max().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_command_builds_openssh_args() {
        let p = Profile {
            kind: "ssh".into(),
            host: "example.com".into(),
            port: 2222,
            user: "yt".into(),
            auth: "key".into(),
            key_path: "/home/user/.ssh/id_ed25519".into(),
            ..Profile::default()
        };
        let (prog, args) = p.ssh_command();
        assert_eq!(prog, "ssh");
        assert_eq!(args, vec!["-i", "/home/user/.ssh/id_ed25519", "-p", "2222", "-l", "yt", "example.com"]);

        let agent = Profile { kind: "ssh".into(), host: "h".into(), port: 22, user: "u".into(), auth: "agent".into(), ..Profile::default() };
        assert_eq!(agent.ssh_command().1, vec!["-p", "22", "-l", "u", "h"]);

        let with_extra = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: "agent".into(),
            extra_args: vec!["-o".into(), "UserKnownHostsFile=/tmp/kh".into()],
            ..Profile::default()
        };
        assert_eq!(
            with_extra.ssh_command().1,
            vec!["-p", "22", "-l", "u", "-o", "UserKnownHostsFile=/tmp/kh", "h"]
        );
    }

    #[test]
    fn ssh_key_path_expands_tilde() {
        let p = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: "key".into(),
            key_path: "~/.ssh/id_ed25519".into(),
            ..Profile::default()
        };
        let (_, args) = p.ssh_command();
        let expected = crate::config::expand_tilde("~/.ssh/id_ed25519").display().to_string();
        assert_eq!(args[1], expected);
    }

    #[test]
    fn password_auth_ssh_command_is_password_only() {
        let p = Profile {
            kind: "ssh".into(),
            host: "127.0.0.1".into(),
            port: 2222,
            user: "test".into(),
            auth: "password".into(),
            save_password: true,
            extra_args: vec!["-o".into(), "UserKnownHostsFile=/tmp/kh".into()],
            ..Profile::default()
        };
        let (prog, args) = p.ssh_command();
        assert_eq!(prog, "ssh");
        assert_eq!(
            args,
            vec![
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "PreferredAuthentications=password,keyboard-interactive",
                "-o",
                "KbdInteractiveAuthentication=yes",
                "-p",
                "2222",
                "-l",
                "test",
                "-o",
                "UserKnownHostsFile=/tmp/kh",
                "127.0.0.1",
            ],
            "auth=password 必须禁用公钥、只走密码（kbd-int 兜底）"
        );
        // 其它认证方式不应被注入。
        for auth in ["agent", "key"] {
            let other = Profile { auth: auth.into(), ..p.clone() };
            let args = other.ssh_command().1;
            assert!(!args.iter().any(|a| a == "PubkeyAuthentication=no"), "auth={auth}");
            assert!(!args.iter().any(|a| a.starts_with("PreferredAuthentications=")), "auth={auth}");
        }
    }

    #[test]
    fn update_keeps_id_and_rejects_duplicate_names() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("a")).unwrap();
        let b = s.add(sample("b")).unwrap();
        let mut edited = sample("a2");
        edited.shell = "/bin/zsh".into();
        s.update(a, edited).unwrap();
        assert_eq!(s.get(a).unwrap().name, "a2");
        assert_eq!(s.get(a).unwrap().shell, "/bin/zsh");
        // 改成别人的名字要拒绝
        assert!(s.update(a, sample("b")).is_err());
        // 空名拒绝
        assert!(s.update(a, sample("  ")).is_err());
        assert_eq!(s.profiles.len(), 2);
        assert_eq!(s.get(b).unwrap().name, "b");
    }

    #[test]
    fn forward_validation_helper() {
        assert!(validate_forward("8080:localhost:80").is_ok());
        assert!(validate_forward("8080:127.0.0.1:80").is_ok());
        assert!(validate_forward("8080").is_err());
        assert!(validate_forward("0:h:80").is_err());
        assert!(validate_forward("8080:h:0").is_err());
        assert!(validate_forward("8080::80").is_err());
    }

    #[test]
    fn ssh_forwards_become_dash_l_args() {
        let p = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            local_forwards: vec!["8080:localhost:80".into(), "5432:db.internal:5432".into()],
            ..Profile::default()
        };
        assert_eq!(
            p.ssh_command().1,
            vec!["-p", "22", "-l", "u", "-L", "8080:localhost:80", "-L", "5432:db.internal:5432", "h"]
        );
        let bad = Profile { local_forwards: vec!["8080".into()], ..p.clone() };
        assert!(bad.validate().unwrap_err().contains("端口转发"));
        let bad_port = Profile { local_forwards: vec!["0:h:80".into()], ..p };
        assert!(bad_port.validate().is_err());
    }

    #[test]
    fn ssh_remote_dynamic_forwards_and_proxy_jump_become_args() {
        let p = Profile {
            kind: "ssh".into(),
            host: "target".into(),
            port: 22,
            user: "u".into(),
            remote_forwards: vec!["8080:localhost:80".into()],
            dynamic_forwards: vec!["1080".into(), "127.0.0.1:1081".into()],
            proxy_jump: " jump@bastion:2222 ".into(),
            agent_forward: true,
            ..Profile::default()
        };
        assert_eq!(
            p.ssh_command().1,
            vec![
                "-A",
                "-J",
                "jump@bastion:2222",
                "-p",
                "22",
                "-l",
                "u",
                "-R",
                "8080:localhost:80",
                "-D",
                "1080",
                "-D",
                "127.0.0.1:1081",
                "target",
            ]
        );
        assert!(p.validate().is_ok(), "合法的新转发/跳板字段应通过校验");

        // 空字段保持旧命令完全不变（additive）。
        let old = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            ..Profile::default()
        };
        assert_eq!(old.ssh_command().1, vec!["-p", "22", "-l", "u", "h"]);
    }

    #[test]
    fn new_ssh_field_validation_rejects_bad_specs() {
        let base = Profile {
            kind: "ssh".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            ..Profile::default()
        };
        for spec in ["8080:localhost:80", "127.0.0.1:8080:localhost:80"] {
            let p = Profile { remote_forwards: vec![spec.into()], ..base.clone() };
            assert!(p.validate().is_ok(), "remote {spec}");
        }
        for spec in ["", "8080", "0:localhost:80", "8080:localhost:0", "a:b"] {
            let p = Profile { remote_forwards: vec![spec.into()], ..base.clone() };
            assert!(p.validate().is_err(), "remote 应拒绝 {spec:?}");
        }
        for spec in ["1080", "127.0.0.1:1080"] {
            let p = Profile { dynamic_forwards: vec![spec.into()], ..base.clone() };
            assert!(p.validate().is_ok(), "dynamic {spec}");
        }
        for spec in ["", "0", "127.0.0.1", "a:b:c"] {
            let p = Profile { dynamic_forwards: vec![spec.into()], ..base.clone() };
            assert!(p.validate().is_err(), "dynamic 应拒绝 {spec:?}");
        }
        for spec in ["", "user@jump:2222", "jump,jump2", "::1"] {
            let p = Profile { proxy_jump: spec.into(), ..base.clone() };
            assert!(p.validate().is_ok(), "jump {spec:?}");
        }
        for spec in ["-oProxyCommand=evil", "jump host"] {
            let p = Profile { proxy_jump: spec.into(), ..base.clone() };
            assert!(p.validate().is_err(), "jump 应拒绝 {spec:?}");
        }
    }

    #[test]
    fn old_profiles_toml_without_new_ssh_fields_still_loads() {
        let dir = tmpdir("additive-ssh");
        let path = dir.join("profiles.toml");
        // 模拟 v2 旧文件：没有 remote_forwards/dynamic_forwards/proxy_jump/agent_forward。
        std::fs::write(
            &path,
            r#"
version = 2
next_id = 2

[[profiles]]
id = 1
name = "old-ssh"
kind = "ssh"
host = "example.com"
port = 2222
user = "yt"
auth = "key"
key_path = "/home/user/.ssh/id_ed25519"
local_forwards = ["8080:localhost:80"]
"#,
        )
        .unwrap();
        let store = ProfileStore::load_from(&path);
        let p = &store.profiles[0];
        assert_eq!(p.name, "old-ssh");
        assert!(p.remote_forwards.is_empty());
        assert!(p.dynamic_forwards.is_empty());
        assert!(p.proxy_jump.is_empty());
        assert!(!p.agent_forward);
        assert_eq!(
            p.ssh_command().1,
            vec![
                "-i",
                "/home/user/.ssh/id_ed25519",
                "-p",
                "2222",
                "-l",
                "yt",
                "-L",
                "8080:localhost:80",
                "example.com",
            ],
            "旧 profiles.toml 读入后命令保持不变"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn telnet_and_serial_commands() {        let t = Profile {
            kind: "telnet".into(),
            host: "10.0.0.1".into(),
            port: 23,
            ..Profile::default()
        };
        assert_eq!(t.telnet_command(), ("telnet".to_string(), vec!["10.0.0.1".to_string(), "23".to_string()]));

        let s = Profile {
            kind: "serial".into(),
            serial_port: "/dev/ttyUSB0".into(),
            baud: 115200,
            ..Profile::default()
        };
        assert_eq!(
            s.serial_command(),
            ("socat".to_string(), vec!["-,raw,echo=0".to_string(), "/dev/ttyUSB0,raw,echo=0,b115200".to_string()])
        );
    }

    #[test]
    fn validate_by_kind() {
        let mut ssh = Profile { name: "s".into(), kind: "ssh".into(), host: String::new(), user: String::new(), ..Profile::default() };
        assert!(ssh.validate().unwrap_err().contains("主机"));
        ssh.host = "example.com".into();
        ssh.user = "yt".into();
        ssh.port = 22;
        assert!(ssh.validate().is_ok());
        ssh.port = 0;
        assert!(ssh.validate().is_err());
        ssh.port = 2222;
        ssh.auth = "key".into();
        assert!(ssh.validate().unwrap_err().contains("私钥"));
        ssh.key_path = "/home/user/.ssh/id_ed25519".into();
        assert!(ssh.validate().is_ok());

        let mut serial = Profile { name: "c".into(), kind: "serial".into(), ..Profile::default() };
        assert!(serial.validate().is_err());
        serial.serial_port = "/dev/ttyUSB0".into();
        assert!(serial.validate().is_ok());

        let mut telnet = Profile { name: "t".into(), kind: "telnet".into(), ..Profile::default() };
        assert!(telnet.validate().is_err());
        telnet.host = "10.0.0.1".into();
        assert!(telnet.validate().is_ok());

        let mut bogus = Profile { name: "x".into(), ..Profile::default() };
        bogus.kind = "carrier-pigeon".into();
        assert!(bogus.validate().unwrap_err().contains("未知"));
    }

    #[test]
    fn v1_file_is_migrated_and_written_back() {
        let dir = tmpdir("v1");
        let path = dir.join("profiles.toml");
        // v1：没有 kind 字段
        std::fs::write(
            &path,
            r#"
version = 1
next_id = 2

[[profiles]]
id = 1
name = "old"
shell = "/bin/sh"
args = []
cwd = "/tmp"
env = []
"#,
        )
        .unwrap();
        let store = ProfileStore::load_from(&path);
        assert_eq!(store.version, VERSION);
        assert_eq!(store.profiles[0].kind, "local");
        // 新增外观字段对 v1 文件是 additive：迁移后为 None（继承全局）。
        assert_eq!(store.profiles[0].font_size, None);
        assert_eq!(store.profiles[0].theme, None);
        // 迁移已写回文件
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("version = 2"), "{text}");
        assert!(text.contains("kind = \"local\""), "{text}");
        assert!(!text.contains("font_size"), "None 不应写出外观键: {text}");
        // 迁移前备份保留原始 v1 内容（回滚/排障用）
        let backup = dir.join("profiles.toml.v1.bak");
        let original = std::fs::read_to_string(&backup).expect("应生成迁移前备份");
        assert!(original.contains("version = 1"), "{original}");
        assert!(!original.contains("version = 2"), "{original}");
        // 迁移后的文件还能再次正常读取，且不会重复备份
        let again = ProfileStore::load_from(&path);
        assert_eq!(again.profiles.len(), 1);
        assert_eq!(again.profiles[0].kind, "local");
        assert!(!dir.join("profiles.toml.v2.bak").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_serialization_has_no_plaintext_password() {
        // Profile 根本没有 password 字段；password 认证只持久化 save_password 标志。
        let p = Profile {
            kind: "ssh".into(),
            host: "example.com".into(),
            port: 22,
            user: "yt".into(),
            auth: "password".into(),
            save_password: true,
            ..Profile::default()
        };
        let text = toml::to_string_pretty(&p).unwrap();
        assert!(!text.contains("hunter2"), "序列化结果不能含明文密码: {text}");
        assert!(text.contains("save_password = true"), "{text}");
        for line in text.lines() {
            if let Some((key, _)) = line.split_once('=') {
                assert_ne!(key.trim(), "password", "不允许出现 password 字段: {line}");
            }
        }
        let back: Profile = toml::from_str(&text).unwrap();
        assert_eq!(back.auth, "password");
        assert!(back.save_password);
        assert!(!Profile::default().save_password, "默认不保存密码");
    }

    #[test]
    fn remove_works_and_new_ids_not_reused() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("a")).unwrap();
        assert!(s.remove(a));
        assert!(!s.remove(a));
        let b = s.add(sample("b")).unwrap();
        assert_ne!(a, b, "删除后 ID 不该复用");
    }

    #[test]
    fn filter_matches_name_host_kind_case_insensitively() {
        let profiles = vec![
            Profile { name: "Work".into(), ..Profile::default() },
            Profile {
                name: "Prod".into(),
                kind: "ssh".into(),
                host: "Example.COM".into(),
                user: "yt".into(),
                ..Profile::default()
            },
            Profile { name: "串口".into(), kind: "serial".into(), ..Profile::default() },
        ];
        // 名字大小写不敏感
        assert_eq!(filter_indices(&profiles, "WORK"), vec![0]);
        assert_eq!(filter_indices(&profiles, "work"), vec![0]);
        // host 也参与匹配
        assert_eq!(filter_indices(&profiles, "example"), vec![1]);
        // kind 也参与匹配
        assert_eq!(filter_indices(&profiles, "SSH"), vec![1]);
        assert_eq!(filter_indices(&profiles, "串"), vec![2]);
        // 空查询 / 全空白 = 恢复全部
        assert_eq!(filter_indices(&profiles, ""), vec![0, 1, 2]);
        assert_eq!(filter_indices(&profiles, "   "), vec![0, 1, 2]);
        // 无命中
        assert!(filter_indices(&profiles, "zzz").is_empty());
    }

    #[test]
    fn default_index_resolves_and_falls_back() {
        let mut s = ProfileStore::default();
        let a = s.add(sample("a")).unwrap();
        let b = s.add(sample("b")).unwrap();
        assert_eq!(s.default_index(a), Some(0));
        assert_eq!(s.default_index(b), Some(1));
        // 0 = 未设置；失效 ID = 回退默认 shell，不 panic
        assert_eq!(s.default_index(0), None);
        assert_eq!(s.default_index(b + 99), None);
        s.remove(a);
        assert_eq!(s.default_index(a), None, "删除默认 Profile 后解析不到");
        assert_eq!(s.default_index(b), Some(0));
    }

    #[test]
    fn duplicate_assigns_new_id_and_dedupes_copy_names() {
        let dir = tmpdir("duplicate");
        let path = dir.join("profiles.toml");
        let mut s = ProfileStore::default();
        let a = s.add(Profile { name: "work".into(), shell: "/bin/zsh".into(), ..Profile::default() }).unwrap();
        let d1 = s.duplicate(a).unwrap();
        let d2 = s.duplicate(a).unwrap();
        assert_ne!(a, d1);
        assert_ne!(d1, d2);
        assert_eq!(s.get(d1).unwrap().name, "work (copy 1)");
        assert_eq!(s.get(d2).unwrap().name, "work (copy 2)");
        // 字段全量复制，ID/名字除外
        assert_eq!(s.get(d1).unwrap().shell, "/bin/zsh");
        assert_eq!(s.get(d1).unwrap().kind, "local");
        // 落盘后仍可读回
        s.save_to(&path);
        let back = ProfileStore::load_from(&path);
        assert_eq!(back.profiles.len(), 3);
        assert_eq!(back.get(d2).unwrap().name, "work (copy 2)");
        assert!(back.next_id > d2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_name_helper_handles_existing_copies() {
        let existing = vec![
            Profile { name: "work (copy 1)".into(), ..Profile::default() },
            Profile { name: "work (copy 2)".into(), ..Profile::default() },
        ];
        assert_eq!(duplicate_name("work", &existing), "work (copy 3)");
        assert_eq!(duplicate_name("  work  ", &[]), "work (copy 1)");
    }

    #[test]
    fn duplicate_missing_profile_is_rejected() {
        let mut s = ProfileStore::default();
        assert!(s.duplicate(42).is_err());
        assert!(s.profiles.is_empty());
    }

    #[test]
    fn custom_title_overrides_tab_title_and_roundtrips() {
        // 空/空白 = 用 Profile 名（不写出一条空标题）。
        let mut p = sample("work");
        assert_eq!(p.effective_title(), "work");
        p.title = Some("  ".into());
        p.normalize_appearance();
        assert_eq!(p.title, None);
        assert_eq!(p.effective_title(), "work");

        // 自定义标题进 Tab、进磁盘、原样读回。
        let dir = tmpdir("title");
        let path = dir.join("profiles.toml");
        let mut s = ProfileStore::default();
        s.add(Profile {
            name: "work".into(),
            title: Some("  prod deploy  ".into()),
            shell: "/bin/zsh".into(),
            ..Profile::default()
        })
        .unwrap();
        assert_eq!(s.profiles[0].effective_title(), "prod deploy");
        s.save_to(&path);
        let back = ProfileStore::load_from(&path);
        assert_eq!(back.profiles[0].title.as_deref(), Some("prod deploy"));
        assert_eq!(back.profiles[0].effective_title(), "prod deploy");

        // 没有 title 的旧文件保持继承 name，且读盘不改写原文件。
        let old = r#"
version = 2
next_id = 2

[[profiles]]
id = 1
name = "legacy"
kind = "local"
"#;
        std::fs::write(&path, old).unwrap();
        let store = ProfileStore::load_from(&path);
        assert_eq!(store.profiles[0].title, None);
        assert_eq!(store.profiles[0].effective_title(), "legacy");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_start_dir_reports_dir_file_and_missing() {
        let dir = tmpdir("cwd-test");
        assert!(test_start_dir("").unwrap_err().contains("为空"));
        assert!(test_start_dir("   ").is_err());
        let ok = test_start_dir(dir.to_str().unwrap()).unwrap();
        assert_eq!(ok, dir);

        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(test_start_dir(file.to_str().unwrap())
            .unwrap_err()
            .contains("不是目录"));

        let missing = dir.join("nope");
        assert!(test_start_dir(missing.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
