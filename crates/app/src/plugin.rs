// SPDX-License-Identifier: GPL-3.0-only
//! 插件清单与外部进程宿主（MVP）。
//!
//! 支持两种清单格式，互不替换：
//!
//! 1. **旧扁平清单**：`<plugins>/*.toml`（`id`/`name`/`version`/`description`/`enabled`）。
//!    仅发现、校验、展示，**不执行**；保留给旧配置与旧版设置页。
//! 2. **v1 目录插件**：`<plugins>/<name>/plugin.toml`，可声明 `[[commands]]`。
//!    启用后命令以 `plugin.<name>.<id>` 注册进命令表；执行时把 `entry` 作为普通子进程
//!    spawn，通过 stdin 发一行 JSON，stdout/stderr 写日志，超时 kill。
//!
//! 安全边界：插件是**普通子进程**，拥有当前用户全部权限，没有沙箱/权限模型；
//! `entry` 只允许插件目录内的相对路径（拒绝绝对路径、`..`、符号链接逃逸）。
//! 详见 `docs/PLUGIN_API.md`。

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::commands::{self, Command, Registry, Shortcut};

/// 宿主实现的插件 API 版本；清单里的 `api` 必须精确等于它。
pub const PLUGIN_API_VERSION: u32 = 1;

/// 单条插件命令的执行超时（MVP 固定值，不开放配置）。
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// stdout/stderr 各最多保留的字节数，防止坏插件撑爆内存/日志。
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

/// 日志里单条流最多展示的字节数。
const MAX_LOG_BYTES: usize = 8 * 1024;

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
        Ok(())
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

    /// 在后台线程执行一条插件命令。返回 false 表示不是已启用的插件命令。
    ///
    /// 绝不阻塞调用方（UI 线程）：spawn 后立即返回，stdout/stderr/退出码只写日志。
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
        let thread_name = format!("plugin-{plugin_name}");
        let spawned = std::thread::Builder::new().name(thread_name).spawn(move || {
            match execute(&plugin_name, &plugin_dir, &entry, &command_id, &[], COMMAND_TIMEOUT) {
                Ok(report) => log_execution(&full_id, &report),
                Err(e) => eprintln!("[plugin] {plugin_name}: 执行 {full_id} 失败: {e}"),
            }
        });
        match spawned {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[plugin] {error_label}: 无法创建执行线程: {e}");
                false
            },
        }
    }
}

/// 一次插件命令执行的结果（仅供日志/测试；MVP 不回传 UI）。
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

/// 执行一次插件 entry（同步）。生产路径由 [`PluginStore::spawn_command`] 放到后台线程；
/// 测试直接调用本函数，不需要启动 GUI。
pub(crate) fn execute(
    plugin_name: &str,
    plugin_dir: &Path,
    entry: &Path,
    command_id: &str,
    args: &[String],
    timeout: Duration,
) -> Result<ExecutionReport, String> {
    let payload = serde_json::json!({
        "event": "command",
        "id": command_id,
        "args": args,
    })
    .to_string()
        + "\n";

    let mut child = ProcessCommand::new(entry)
        .current_dir(plugin_dir)
        .env("TERVO_PLUGIN_NAME", plugin_name)
        .env("TERVO_PLUGIN_API", PLUGIN_API_VERSION.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 {}: {e}", entry.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        // 写失败（插件提前退出）不是宿主错误；后续按退出码记录。
        let _ = stdin.write_all(payload.as_bytes());
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
}
