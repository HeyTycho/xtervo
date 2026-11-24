// SPDX-License-Identifier: GPL-3.0-only
//! MCP（Model Context Protocol）配置与运行期权限。
//!
//! MCP 是内嵌功能，不是插件。配置是扁平 TOML 的五个布尔值：
//! `mcp_enabled`、`mcp_read`、`mcp_input`、`mcp_tabs`、`mcp_destructive`。
//! 默认 `enabled + read`，写分组默认关闭，因此默认只读。
//!
//! 权限分组（见 docs/MCP_TERMINAL_CONTROL.md）：
//! - read:        capabilities / snapshot / list / output
//! - input:       send_text / send_key / run
//! - tabs:        new_tab / split / focus / scroll
//! - destructive: close_pane / close_tab / kill
//!
//! destructive 是**附加**开关：close_* 还要求 tabs，kill 还要求 input。
//! 旧的 `XTERVO_DIAG_ALLOW_WRITE` / `XTERVO_DIAG_ALLOW_DESTRUCTIVE` 环境变量
//! 仍可放宽对应分组（测试/CI 兼容），但不能绕过 `mcp_enabled=false`。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};

use crate::config::Config;

pub const SOCKET_ENV: &str = "XTERVO_DIAG_SOCKET";
pub const TOKEN_ENV: &str = "XTERVO_DIAG_TOKEN";
pub const WRITE_ENV: &str = "XTERVO_DIAG_ALLOW_WRITE";
pub const DESTRUCTIVE_ENV: &str = "XTERVO_DIAG_ALLOW_DESTRUCTIVE";

pub const SOCKET_FILE_NAME: &str = "mcp.sock";
pub const TOKEN_FILE_NAME: &str = "mcp.token";
const TOKEN_BYTES: usize = 32;
const MAX_TOKEN_BYTES: usize = 512;
/// Unix socket path 的实际上限通常是 108 字节；留出余量。
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// 命令所属的权限分组。`capabilities` 属于 read；MCP bridge 的内部探测
/// （`probe:true`）单独允许，见 [`McpPermissions::allows_probe`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpGroup {
    Read,
    Input,
    Tabs,
    Destructive,
}

impl McpGroup {
    pub const ALL: [Self; 4] = [Self::Read, Self::Input, Self::Tabs, Self::Destructive];

    /// 该分组暴露的 socket 命令。
    pub fn commands(self) -> &'static [&'static str] {
        match self {
            Self::Read => &["capabilities", "snapshot", "list", "output"],
            Self::Input => &["send_text", "send_key", "run"],
            Self::Tabs => &["new_tab", "new_window", "split", "focus", "scroll"],
            Self::Destructive => &["close_pane", "close_tab", "close_window", "kill"],
        }
    }
}

/// 全部 socket 命令的稳定顺序；`capabilities` 只在 read 分组允许时出现。
pub const ALL_COMMANDS: [&str; 16] = [
    "capabilities",
    "snapshot",
    "list",
    "output",
    "send_text",
    "send_key",
    "run",
    "new_tab",
    "new_window",
    "split",
    "focus",
    "scroll",
    "close_pane",
    "close_tab",
    "close_window",
    "kill",
];

/// 一条命令属于哪个分组。destructive 命令同时依赖基础分组，见
/// [`McpPermissions::allows_command`]。
pub fn group_for_command(command: &str) -> Option<McpGroup> {
    match command {
        "capabilities" | "snapshot" | "list" | "output" => Some(McpGroup::Read),
        "send_text" | "send_key" | "run" => Some(McpGroup::Input),
        "new_tab" | "new_window" | "split" | "focus" | "scroll" => Some(McpGroup::Tabs),
        "close_pane" | "close_tab" | "close_window" | "kill" => {
            Some(McpGroup::Destructive)
        },
        _ => None,
    }
}

/// 一份生效的 MCP 权限快照（配置 + 环境变量覆盖）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpPermissions {
    pub enabled: bool,
    pub read: bool,
    pub input: bool,
    pub tabs: bool,
    pub destructive: bool,
}

impl Default for McpPermissions {
    fn default() -> Self {
        Self {
            enabled: true,
            read: true,
            input: false,
            tabs: false,
            destructive: false,
        }
    }
}

impl McpPermissions {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.mcp_enabled,
            read: cfg.mcp_read,
            input: cfg.mcp_input,
            tabs: cfg.mcp_tabs,
            destructive: cfg.mcp_destructive,
        }
    }

    /// 旧环境变量 override：write 同时放宽 input/tabs；destructive 只放宽
    /// destructive 本身，close/kill 仍要求对应基础分组。不能覆盖 enabled。
    pub fn widened(mut self, write: bool, destructive: bool) -> Self {
        self.input |= write;
        self.tabs |= write;
        self.destructive |= destructive;
        self
    }

    /// 服务是否允许内部 capability 探测（即使 read 关闭）。
    pub fn allows_probe(&self) -> bool {
        self.enabled
    }

    pub fn allows_command(&self, command: &str) -> bool {
        if !self.enabled {
            return false;
        }
        match command {
            "capabilities" | "snapshot" | "list" | "output" => self.read,
            "send_text" | "send_key" | "run" => self.input,
            "new_tab" | "new_window" | "split" | "focus" | "scroll" => self.tabs,
            // destructive 不是“再加一个分组就全开”：close 需要 tabs，kill 需要 input。
            "close_pane" | "close_tab" | "close_window" => self.tabs && self.destructive,
            "kill" => self.input && self.destructive,
            _ => false,
        }
    }

    /// 只读 = 没有任何写分组。注意 read 本身可能被关闭；那是“无 live 工具”。
    pub fn read_only(&self) -> bool {
        !self.input && !self.tabs && !self.destructive
    }

    /// 实际暴露的 live 命令（按稳定顺序）。
    pub fn commands(&self) -> Vec<&'static str> {
        ALL_COMMANDS
            .iter()
            .copied()
            .filter(|command| self.allows_command(command))
            .collect()
    }

    /// `live_capabilities` / `tools/list` 共用的数据结构。绝不包含 token。
    pub fn capabilities_json(&self) -> Value {
        json!({
            "schema": "xtervo.diag.v1",
            "live": true,
            "enabled": self.enabled,
            "read_only": self.read_only(),
            "read_enabled": self.read,
            "write_enabled": self.input || self.tabs,
            "input_enabled": self.input,
            "tabs_enabled": self.tabs,
            "destructive_enabled": self.destructive,
            "groups": {
                "read": self.read,
                "input": self.input,
                "tabs": self.tabs,
                "destructive": self.destructive,
            },
            "commands": self.commands(),
        })
    }

    /// 拒绝原因（用于 socket 错误和测试）。已允许时返回 `None`。
    pub fn denial_reason(&self, command: &str) -> Option<&'static str> {
        if self.allows_command(command) {
            return None;
        }
        if !self.enabled {
            return Some("MCP disabled (enable MCP in Settings → Privacy)");
        }
        match command {
            "capabilities" | "snapshot" | "list" | "output" => {
                Some("read disabled (enable MCP read)")
            },
            "send_text" | "send_key" | "run" => Some("input disabled (enable MCP input)"),
            "new_tab" | "new_window" | "split" | "focus" | "scroll" => {
                Some("tabs disabled (enable MCP tabs)")
            },
            "close_pane" | "close_tab" | "close_window" if !self.tabs => {
                Some("tabs disabled (destructive close also requires MCP tabs)")
            },
            "close_pane" | "close_tab" | "close_window" => {
                Some("destructive disabled (enable MCP destructive)")
            },
            "kill" if !self.input => {
                Some("input disabled (destructive kill also requires MCP input)")
            },
            "kill" => Some("destructive disabled (enable MCP destructive)"),
            _ => Some("unknown control command"),
        }
    }
}

/// MCP 运行期状态：权限可热更新，token 只在进程内保存。
pub struct McpRuntime {
    permissions: RwLock<McpPermissions>,
    token: Mutex<Option<String>>,
    token_path: PathBuf,
    /// 上次看到的 token 文件 mtime；变化时按需重读（运行中替换/收紧权限）。
    token_mtime: Mutex<Option<std::time::SystemTime>>,
    socket_path: PathBuf,
    env_token: bool,
    env_write: bool,
    env_destructive: bool,
    active: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl McpRuntime {
    pub fn new(cfg: &Config) -> Self {
        let env_token = env_value(TOKEN_ENV);
        let env_write = env_flag(WRITE_ENV);
        let env_destructive = env_flag(DESTRUCTIVE_ENV);
        let permissions = McpPermissions::from_config(cfg).widened(env_write, env_destructive);
        let socket_path = socket_path_from_env_or_default();
        let token_path = token_file_path();
        let mut last_error = None;
        let token = match env_token.clone() {
            Some(token) => Some(token),
            None if permissions.enabled => match ensure_token_file(&token_path) {
                Ok(token) => Some(token),
                Err(error) => {
                    let message = format!("state token 不可用: {error}");
                    eprintln!("[mcp] {message}");
                    last_error = Some(message);
                    None
                },
            },
            None => None,
        };
        let token_mtime = fs::metadata(&token_path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        let runtime = Self {
            permissions: RwLock::new(permissions),
            token: Mutex::new(token),
            token_path,
            token_mtime: Mutex::new(token_mtime),
            socket_path,
            env_token: env_token.is_some(),
            env_write,
            env_destructive,
            active: AtomicBool::new(false),
            last_error: Mutex::new(last_error),
        };
        eprintln!(
            "[mcp] enabled={} read={} input={} tabs={} destructive={} socket={} token={}",
            permissions.enabled,
            permissions.read,
            permissions.input,
            permissions.tabs,
            permissions.destructive,
            runtime.socket_path.display(),
            runtime.token_source(),
        );
        if env_write || env_destructive {
            eprintln!(
                "[mcp] env override 生效: {WRITE_ENV}={} {DESTRUCTIVE_ENV}={}（配置仍可收紧分组，但 env 会放宽）",
                env_write, env_destructive
            );
        }
        runtime
    }

    pub fn permissions(&self) -> McpPermissions {
        *self
            .permissions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 配置热更新：立即换权限；从关闭切到开启时按需补 state token。
    /// token 值绝不写日志/配置。
    pub fn apply_config(&self, cfg: &Config) {
        let next = McpPermissions::from_config(cfg)
            .widened(self.env_write, self.env_destructive);
        let changed = {
            let mut guard = self
                .permissions
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = *guard != next;
            *guard = next;
            changed
        };
        if changed {
            eprintln!(
                "[mcp] 权限更新 enabled={} read={} input={} tabs={} destructive={}",
                next.enabled, next.read, next.input, next.tabs, next.destructive
            );
        }
        // 每次启用/权限更新都重读 state token：磁盘被替换、权限被收紧或
        // 先禁用再启用时，不能继续用内存里的旧值。
        if next.enabled && !self.env_token {
            match ensure_token_file(&self.token_path) {
                Ok(token) => {
                    let old = self
                        .token
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .replace(token.clone());
                    if old.as_deref() != Some(token.as_str()) {
                        eprintln!("[mcp] state token 已从磁盘重新加载");
                    }
                    *self.token_mtime.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        fs::metadata(&self.token_path)
                            .ok()
                            .and_then(|meta| meta.modified().ok());
                },
                Err(error) => {
                    let message = format!("state token 不可用: {error}");
                    eprintln!("[mcp] {message}");
                    self.set_error(Some(message));
                },
            }
        }
    }

    pub fn token(&self) -> Option<String> {
        if !self.env_token {
            refresh_state_token(&self.token_path, &self.token_mtime, &self.token);
        }
        self.token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn token_source(&self) -> &'static str {
        if self.token().is_some() {
            if self.env_token {
                "env"
            } else {
                "state-file"
            }
        } else {
            "unavailable"
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn token_path(&self) -> &Path {
        &self.token_path
    }

    pub fn env_write_override(&self) -> bool {
        self.env_write
    }

    pub fn env_destructive_override(&self) -> bool {
        self.env_destructive
    }

    pub fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 同一个错误只记一行，避免 socket 重试刷屏。
    pub fn set_error(&self, error: Option<String>) {
        let mut slot = self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.as_deref() != error.as_deref() {
            if let Some(message) = &error {
                eprintln!("[mcp] {message}");
            }
            *slot = error;
        }
    }

    pub fn clear_error(&self) {
        self.set_error(None);
    }
}

static RUNTIME: OnceLock<Arc<McpRuntime>> = OnceLock::new();

/// GUI 启动时调用一次。`xtervo diag serve` 不初始化全局 runtime。
pub fn init(cfg: &Config) -> Arc<McpRuntime> {
    let runtime = Arc::new(McpRuntime::new(cfg));
    let _ = RUNTIME.set(runtime.clone());
    runtime
}

pub fn runtime() -> Option<Arc<McpRuntime>> {
    RUNTIME.get().cloned()
}

/// 设置页/外部 config 热更新统一入口。
pub fn apply_config(cfg: &Config) {
    if let Some(runtime) = RUNTIME.get() {
        runtime.apply_config(cfg);
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// 配置文件存在且解析成功时返回其 mcp_enabled；缺失/解析失败按默认 true。
/// 用 `parse_text` 避免 `xtervo diag serve` 触发配置迁移写回。
pub fn config_enabled_from_disk() -> bool {
    match fs::read_to_string(Config::path()) {
        Ok(text) => config_enabled_from_text(&text),
        // 文件不存在 = 首次运行，沿用默认开启；存在但不可读 = 无法确认
        // 用户是否写了 false，按安全侧关闭。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn config_enabled_from_text(text: &str) -> bool {
    Config::parse_text(text)
        .map(|cfg| cfg.mcp_enabled)
        // 坏配置无法证明用户开启过 MCP：fail closed。
        .unwrap_or(false)
}

/// `$XDG_RUNTIME_DIR/xtervo/mcp.sock`；没有 XDG_RUNTIME_DIR 时退回
/// `$XDG_STATE_HOME/xtervo/mcp.sock`（或 `~/.local/state/xtervo/mcp.sock`）。
/// 路径过长时退到 `$TMPDIR/xtervo-<uid>/mcp.sock`（Unix socket 108 字节限制）。
pub fn default_socket_path() -> PathBuf {
    socket_path_for(
        env_path("XDG_RUNTIME_DIR").as_deref(),
        &state_dir(),
        &temp_state_dir(),
    )
}

fn socket_path_for(runtime_dir: Option<&Path>, state_dir: &Path, temp_dir: &Path) -> PathBuf {
    if let Some(runtime_dir) = runtime_dir {
        let path = runtime_dir.join("xtervo").join(SOCKET_FILE_NAME);
        if path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
            return path;
        }
    }
    let path = state_dir.join(SOCKET_FILE_NAME);
    if path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return path;
    }
    temp_dir.join(SOCKET_FILE_NAME)
}

pub fn state_dir() -> PathBuf {
    state_dir_for(
        env_path("XDG_STATE_HOME").as_deref(),
        env_path("HOME").as_deref(),
        &temp_state_dir(),
    )
}

fn state_dir_for(xdg_state_home: Option<&Path>, home: Option<&Path>, temp_dir: &Path) -> PathBuf {
    match (xdg_state_home, home) {
        (Some(base), _) => base.join("xtervo"),
        (None, Some(home)) => home.join(".local").join("state").join("xtervo"),
        (None, None) => temp_dir.to_path_buf(),
    }
}

fn temp_state_dir() -> PathBuf {
    std::env::temp_dir().join(format!("xtervo-{}", effective_uid()))
}

pub fn token_file_path() -> PathBuf {
    token_file_path_in(&state_dir())
}

fn token_file_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(TOKEN_FILE_NAME)
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// app 与 `diag serve` 共用：显式 env 覆盖优先，否则默认路径。
pub fn socket_path_from_env_or_default() -> PathBuf {
    env_path(SOCKET_ENV).unwrap_or_else(default_socket_path)
}

/// 确保 socket/token 的父目录存在。只在**新建**目录时 chmod 0700，
/// 不擅自改用户已有目录（例如 XTERVO_DIAG_SOCKET 指向自己的目录）。
pub fn prepare_socket_dir(path: &Path) -> io::Result<()> {
    ensure_parent_dir(path)
}

fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent() else { return Ok(()) };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    if parent.exists() {
        // 已存在的目录可能是别人预建的（尤其是 /tmp/xtervo-<uid> 回退路径）：
        // 必须是当前用户拥有的真实目录，不能是 symlink。
        let meta = fs::symlink_metadata(parent)?;
        if !meta.file_type().is_dir() || meta.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "token/socket directory is not owned by the current user",
            ));
        }
        return Ok(());
    }
    fs::create_dir_all(parent)?;
    set_dir_mode(parent, 0o700)
}

/// 读取 state token；文件必须是非 symlink 普通文件，内容非空且无空白。
pub fn read_token_file(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.len() > MAX_TOKEN_BYTES as u64
    {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    valid_token(text.trim())
}

/// 运行中 token 文件被替换/删除时重读；返回是否发生变化。
fn refresh_state_token(
    path: &Path,
    seen: &std::sync::Mutex<Option<std::time::SystemTime>>,
    token: &std::sync::Mutex<Option<String>>,
) -> bool {
    let mtime = fs::metadata(path).ok().and_then(|meta| meta.modified().ok());
    {
        let guard = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *guard == mtime {
            return false;
        }
    }
    // token 文件被删除 = 撤销；存在但无效 = None。不要继续用旧 token。
    let next = read_token_file(path);
    let mut guard = token.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let changed = *guard != next;
    *guard = next;
    let mut seen_guard = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *seen_guard = mtime;
    if changed {
        eprintln!("[mcp] state token 文件已变化，已重新加载/撤销");
    }
    changed
}

fn valid_token(token: &str) -> Option<String> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_BYTES
        || token
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }
    Some(token.to_string())
}

/// 自动生成/复用 state token（0600）。并发创建时 create_new 的赢家生效，
/// 输家读取同一个文件，保证 app 和 bridge 看到相同 token。
pub fn ensure_token_file(path: &Path) -> io::Result<String> {
    ensure_parent_dir(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::MetadataExt;
            if !metadata.file_type().is_file() || metadata.uid() != effective_uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "token path is not a regular file owned by the current user",
                ));
            }
            if metadata.len() > MAX_TOKEN_BYTES as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "token file exceeds size limit",
                ));
            }
            set_file_mode(path, 0o600)?;
            match fs::read(path) {
                Ok(bytes) => match String::from_utf8(bytes).ok().and_then(|text| valid_token(text.trim())) {
                    Some(token) => Ok(token),
                    None => recreate_token(path),
                },
                Err(error) if error.kind() == io::ErrorKind::InvalidData => recreate_token(path),
                Err(error) => Err(error),
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_token(path),
        Err(error) => Err(error),
    }
}

fn recreate_token(path: &Path) -> io::Result<String> {
    let _ = fs::remove_file(path);
    create_token(path)
}

fn create_token(path: &Path) -> io::Result<String> {
    let token = random_token();
    // 不能 create_new 目标路径后慢慢写：并发读者会看到空/半截 token，触发
    // 删除重建，最终两个进程拿到不同 token。先写唯一临时文件并 fsync，
    // 再用 hard_link 原子发布（已存在则读赢家的完整文件）。
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mcp.token".into());
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        random_token()
    ));
    let write_result = (|| -> io::Result<()> {
        let mut file = open_token_file(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        set_file_mode(&tmp, 0o600)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    match fs::hard_link(&tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(token)
        },
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "token path is not a regular file",
                ));
            }
            let bytes = fs::read(path)?;
            String::from_utf8(bytes)
                .ok()
                .and_then(|text| valid_token(text.trim()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid token file"))
        },
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        },
    }
}

#[cfg(unix)]
fn open_token_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_token_file(path: &Path) -> io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn random_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    token
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid 无参数、无副作用，只读取当前进程的 effective uid。
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perms(read: bool, input: bool, tabs: bool, destructive: bool) -> McpPermissions {
        McpPermissions {
            enabled: true,
            read,
            input,
            tabs,
            destructive,
        }
    }

    fn config_with(
        enabled: bool,
        read: bool,
        input: bool,
        tabs: bool,
        destructive: bool,
    ) -> Config {
        Config {
            mcp_enabled: enabled,
            mcp_read: read,
            mcp_input: input,
            mcp_tabs: tabs,
            mcp_destructive: destructive,
            ..Config::default()
        }
    }

    #[test]
    fn defaults_are_enabled_read_only() {
        let cfg = Config::default();
        let permissions = McpPermissions::from_config(&cfg);
        assert_eq!(
            permissions,
            McpPermissions {
                enabled: true,
                read: true,
                input: false,
                tabs: false,
                destructive: false,
            }
        );
        assert_eq!(
            permissions.commands(),
            vec!["capabilities", "snapshot", "list", "output"]
        );
        assert!(permissions.allows_command("list"));
        assert!(!permissions.allows_command("send_text"));
        assert!(!permissions.allows_command("new_tab"));
        assert!(!permissions.allows_command("close_pane"));
        assert!(!permissions.allows_command("kill"));
    }

    #[test]
    fn command_groups_map_to_expected_commands() {
        assert_eq!(group_for_command("snapshot"), Some(McpGroup::Read));
        assert_eq!(group_for_command("send_text"), Some(McpGroup::Input));
        assert_eq!(group_for_command("split"), Some(McpGroup::Tabs));
        assert_eq!(group_for_command("kill"), Some(McpGroup::Destructive));
        assert_eq!(group_for_command("rename"), None);
        for group in McpGroup::ALL {
            for command in group.commands() {
                assert_eq!(group_for_command(command), Some(group), "{command}");
            }
        }
    }

    #[test]
    fn groups_can_be_enabled_independently() {
        let input_only = perms(false, true, false, false);
        assert_eq!(
            input_only.commands(),
            vec!["send_text", "send_key", "run"]
        );
        assert!(input_only.allows_command("send_text"));
        assert!(!input_only.allows_command("capabilities"));
        assert!(!input_only.allows_command("list"));
        assert!(!input_only.allows_command("kill"));

        let tabs_only = perms(false, false, true, false);
        assert_eq!(
            tabs_only.commands(),
            vec!["new_tab", "new_window", "split", "focus", "scroll"]
        );
        assert!(!tabs_only.allows_command("close_pane"));
    }

    #[test]
    fn destructive_requires_base_group() {
        // destructive 单独打开不授予任何破坏性命令。
        let destructive_only = perms(false, false, false, true);
        assert!(destructive_only.commands().is_empty());
        assert!(!destructive_only.allows_command("close_pane"));
        assert!(!destructive_only.allows_command("kill"));

        // close 需要 tabs + destructive；kill 需要 input + destructive。
        let close = perms(false, false, true, true);
        assert!(close.allows_command("close_pane"));
        assert!(close.allows_command("close_tab"));
        assert!(close.allows_command("close_window"));
        assert!(!close.allows_command("kill"));

        let kill = perms(false, true, false, true);
        assert!(kill.allows_command("kill"));
        assert!(!kill.allows_command("close_pane"));
        assert!(!kill.allows_command("close_tab"));
        assert!(!kill.allows_command("close_window"));
    }

    #[test]
    fn master_switch_denies_everything() {
        let disabled = McpPermissions {
            enabled: false,
            read: true,
            input: true,
            tabs: true,
            destructive: true,
        };
        assert!(disabled.commands().is_empty());
        for command in ALL_COMMANDS {
            assert!(!disabled.allows_command(command), "{command}");
        }
        assert!(disabled.denial_reason("list").unwrap().contains("MCP disabled"));
        assert!(!disabled.allows_probe());
    }

    #[test]
    fn env_overrides_widen_input_tabs_and_destructive() {
        let base = McpPermissions::from_config(&Config::default());
        let write = base.widened(true, false);
        assert!(write.input && write.tabs);
        assert!(!write.destructive);
        assert!(write.allows_command("send_text"));
        assert!(write.allows_command("new_tab"));
        assert!(!write.allows_command("close_pane"));

        let destructive = base.widened(false, true);
        assert!(destructive.destructive);
        // 仍缺基础分组：close/kill 不能被单独放行。
        assert!(!destructive.allows_command("close_pane"));
        assert!(!destructive.allows_command("kill"));

        let both = base.widened(true, true);
        assert!(both.allows_command("close_pane"));
        assert!(both.allows_command("kill"));
    }

    #[test]
    fn capabilities_json_tracks_groups_without_token() {
        let permissions = perms(true, true, true, true);
        let value = permissions.capabilities_json();
        assert_eq!(value["schema"], "xtervo.diag.v1");
        assert_eq!(value["live"], true);
        assert_eq!(value["read_only"], false);
        assert_eq!(value["input_enabled"], true);
        assert_eq!(value["tabs_enabled"], true);
        assert_eq!(value["destructive_enabled"], true);
        assert_eq!(value["commands"].as_array().unwrap().len(), ALL_COMMANDS.len());
        let text = value.to_string();
        assert!(!text.contains("token"), "{text}");
        assert!(!text.contains(TOKEN_FILE_NAME), "{text}");
    }

    #[test]
    fn denial_reasons_name_the_group() {
        assert!(perms(true, false, false, false)
            .denial_reason("run")
            .unwrap()
            .contains("input disabled"));
        assert!(perms(true, false, false, false)
            .denial_reason("new_tab")
            .unwrap()
            .contains("tabs disabled"));
        assert!(perms(true, false, false, false)
            .denial_reason("close_pane")
            .unwrap()
            .contains("tabs disabled"));
        assert!(perms(true, false, false, false)
            .denial_reason("kill")
            .unwrap()
            .contains("input disabled"));
        assert!(perms(true, false, true, false)
            .denial_reason("close_tab")
            .unwrap()
            .contains("destructive disabled"));
        assert!(perms(true, false, false, false)
            .denial_reason("rename")
            .unwrap()
            .contains("unknown"));
        assert_eq!(perms(true, true, true, true).denial_reason("close_pane"), None);
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_and_falls_back_to_state() {
        let runtime = Path::new("/run/user/1000");
        let state = Path::new("/home/user/.local/state/xtervo");
        let temp = Path::new("/tmp/xtervo-1000");
        assert_eq!(
            socket_path_for(Some(runtime), state, temp),
            PathBuf::from("/run/user/1000/xtervo/mcp.sock")
        );
        assert_eq!(
            socket_path_for(None, state, temp),
            PathBuf::from("/home/user/.local/state/xtervo/mcp.sock")
        );
        assert_eq!(
            token_file_path_in(state),
            PathBuf::from("/home/user/.local/state/xtervo/mcp.token")
        );
    }

    #[test]
    fn state_dir_prefers_xdg_state_home_then_home_then_temp() {
        let xdg = Path::new("/x/state");
        let home = Path::new("/home/user");
        let temp = Path::new("/tmp/xtervo-1000");
        assert_eq!(state_dir_for(Some(xdg), Some(home), temp), xdg.join("xtervo"));
        assert_eq!(
            state_dir_for(None, Some(home), temp),
            PathBuf::from("/home/user/.local/state/xtervo")
        );
        assert_eq!(state_dir_for(None, None, temp), temp);
    }

    #[test]
    fn config_flags_are_flat_and_defaults_apply_to_old_configs() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(text.contains("mcp_enabled = true"), "{text}");
        assert!(text.contains("mcp_read = true"), "{text}");
        assert!(text.contains("mcp_input = false"), "{text}");
        assert!(text.contains("mcp_tabs = false"), "{text}");
        assert!(text.contains("mcp_destructive = false"), "{text}");
        assert!(!text.contains("[mcp]"), "{text}");

        let old: Config = toml::from_str("version = 1\ntheme = \"nord\"\n").unwrap();
        assert!(old.mcp_enabled);
        assert!(old.mcp_read);
        assert!(!old.mcp_input && !old.mcp_tabs && !old.mcp_destructive);
        let v0: Config = toml::from_str("theme = \"nord\"\n").unwrap();
        assert!(v0.mcp_enabled && v0.mcp_read);
        assert!(!v0.mcp_input && !v0.mcp_tabs && !v0.mcp_destructive);
    }

    #[test]
    fn config_enabled_from_text_is_conservative() {
        assert!(config_enabled_from_text("version = 1\n"));
        assert!(!config_enabled_from_text(
            "version = 1\nmcp_enabled = false\n"
        ));
        // 坏配置 fail closed：无法确认 mcp_enabled=false 时不能开服务。
        assert!(!config_enabled_from_text("not valid toml"));
    }

    #[test]
    fn runtime_token_reloads_when_file_changes() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-mcp-token-reload-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.token");
        let first = ensure_token_file(&path).unwrap();
        let seen = std::sync::Mutex::new(
            std::fs::metadata(&path)
                .ok()
                .and_then(|meta| meta.modified().ok()),
        );
        let token = std::sync::Mutex::new(Some(first.clone()));
        assert!(!refresh_state_token(&path, &seen, &token));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "second-token\n").unwrap();
        assert!(refresh_state_token(&path, &seen, &token));
        assert_eq!(token.lock().unwrap().as_deref(), Some("second-token"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::remove_file(&path).unwrap();
        assert!(refresh_state_token(&path, &seen, &token));
        assert!(token.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_token_creation_sees_one_valid_token() {
        let dir = std::env::temp_dir().join(format!(
            "xtervo-mcp-token-race-{}-{}",
            std::process::id(),
            crate::logging::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.token");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || ensure_token_file(&path)));
        }
        let tokens: Vec<String> = handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
        assert!(tokens.windows(2).all(|w| w[0] == w[1]), "{tokens:?}");
        assert_eq!(read_token_file(&path).as_deref(), Some(tokens[0].as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_is_0600_reused_and_symlink_safe() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.token");
        let token = ensure_token_file(&path).unwrap();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token must be 0600");

        // 复用而不是重新生成。
        assert_eq!(ensure_token_file(&path).unwrap(), token);
        // 权限被放宽时重新收紧。
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ensure_token_file(&path).unwrap(), token);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // symlink 必须拒绝，且不能改动目标文件。
        let target = dir.path().join("target.txt");
        fs::write(&target, "do-not-touch").unwrap();
        let link = dir.path().join("link.token");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(ensure_token_file(&link).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "do-not-touch");
    }

    #[test]
    fn token_file_is_regenerated_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.token");
        fs::write(&path, "").unwrap();
        let token = ensure_token_file(&path).unwrap();
        assert_eq!(token.len(), TOKEN_BYTES * 2);
        assert_eq!(ensure_token_file(&path).unwrap(), token);
    }
}
