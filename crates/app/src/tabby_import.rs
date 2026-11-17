// SPDX-License-Identifier: GPL-3.0-only
//! Tabby `config.yaml` 的尽力导入/导出。
//!
//! Tabby 的配置结构随版本变化，这里以 2026-10-06 的 `master`
//! （commit `467ce8e`，`tabby-core` 1.0.231-nightly.0，配置 `version: 1`）源码为准：
//! - `terminal.colorScheme` / `terminal.lightColorScheme`：单个完整配色对象；
//! - `terminal.customColorSchemes`：用户保存/粘贴的配色数组；
//! - `hotkeys`：命令 id → `string | string[] | (string|string[])[]` 的映射，
//!   `profile.*` / `profile-selectors.*` / `group-selectors.*` 是动态嵌套键。
//!
//! 只映射语义明确的子集：
//! - Profile：local / ssh / serial / telnet（密码等秘密字段不导入）；
//! - `terminal.fontSize`；
//! - 常见命令的快捷键（见 [`TABBY_HOTKEY_MAP`]），只取每个命令第一个可用的
//!   单段 chord，Tabby 的 multi-chord 序列与一个命令多个候选键无法完整表达；
//! - 配色：16 ANSI + fg/bg/cursor，写成 `themes/tabby-<slug>.toml`，界面 chrome
//!   由 `UiColors::derive` 推导（Tabby 的 selection 等字段忽略）。
//!
//! 未识别的命令、非法颜色、冲突绑定都进入报告，不静默丢弃；重名主题/Profile
//! 加后缀，绝不覆盖已有文件或已有自定义快捷键。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_yaml::{Mapping, Value};
use tervo_core::{PaletteSpec, UiColors};

use crate::commands::{self, Chord, Mods, Registry};
use crate::config::Config;
use crate::profile::{Profile, ProfileStore};
use crate::themes;

/// 导入结果（用于 CLI 输出与 UI 状态行）。
#[derive(Debug, Default)]
pub struct ImportReport {
    pub added: usize,
    /// 重名后自动改名（`name (tabby 2)`）的数量。
    pub renamed: usize,
    /// (Profile 名, 跳过原因)
    pub skipped: Vec<(String, String)>,
    /// 带过来的字号（仅当 Tabby 配置里有 `terminal.fontSize`）。
    pub font_size: Option<f32>,
    /// 新增的 `[[hotkeys]]` 条数。
    pub hotkeys_added: usize,
    /// 与现有默认/自定义绑定相同、无需写入的条数。
    pub hotkeys_unchanged: usize,
    /// Tabby 有、但本导入器没有语义明确映射的命令 id。
    pub hotkeys_unmapped: Vec<String>,
    /// (Tabby 命令 id, 跳过原因)
    pub hotkeys_skipped: Vec<(String, String)>,
    /// 一个命令在 Tabby 里有多个候选键时，除已采用者外忽略的数量。
    pub hotkeys_extra_ignored: usize,
    /// 新写出的主题文件数。
    pub themes_added: usize,
    /// 因文件名已存在而加后缀的主题数。
    pub themes_renamed: usize,
    /// (主题名, 跳过原因)
    pub themes_skipped: Vec<(String, String)>,
    /// 明确**不导入**的类别（插件配置 / UI 主题 CSS / 密码凭据）。
    ///
    /// 与 `skipped`（想导但失败了）不同：这些内容在当前 Tervo 里没有对应模型，
    /// 报告出来是为了不让用户以为“导入成功 = 全都搬过来了”。
    pub not_imported: Vec<NotImported>,
}

/// 一条“明确不导入”的报告项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotImported {
    /// 稳定类别：`plugin-config` / `ui-theme-css` / `custom-css` / `credentials`。
    pub category: String,
    /// 用户可见说明（含来源字段，绝不回显密码值）。
    pub detail: String,
}

impl ImportReport {
    /// 单行“明确未导入”摘要（空 = 没有此类内容）。给设置页状态行用。
    pub fn not_imported_summary(&self, zh: bool) -> String {
        if self.not_imported.is_empty() {
            return String::new();
        }
        let kinds: Vec<&str> = {
            let mut v: Vec<&str> = Vec::new();
            for item in &self.not_imported {
                if !v.contains(&item.category.as_str()) {
                    v.push(item.category.as_str());
                }
            }
            v
        };
        let names = kinds
            .iter()
            .map(|c| match (*c, zh) {
                ("plugin-config", true) => "插件配置",
                ("plugin-config", false) => "plugin configs",
                ("ui-theme-css", true) => "UI 主题 CSS",
                ("ui-theme-css", false) => "UI theme CSS",
                ("custom-css", true) => "自定义 CSS",
                ("custom-css", false) => "custom CSS",
                ("credentials", true) => "密码/凭据",
                ("credentials", false) => "passwords/credentials",
                (other, _) => other,
            })
            .collect::<Vec<_>>()
            .join(if zh { "、" } else { ", " });
        if zh {
            format!("明确不导入 {} 项（{names}）", self.not_imported.len())
        } else {
            format!("{} item(s) intentionally not imported ({names})", self.not_imported.len())
        }
    }
}

/// Tabby 命令 id → Tervo 命令 id。
///
/// 只收语义能一一对应、且 Tervo 确实实现了的命令；Tabby 的 profile/group
/// 选择器、tab 序号、pane 导航方向、滚动、MRU 切换等没有等价命令，保持未映射。
pub const TABBY_HOTKEY_MAP: &[(&str, &str)] = &[
    ("new-tab", "tab.new"),
    ("close-tab", "tab.close"),
    ("next-tab", "tab.next"),
    ("previous-tab", "tab.prev"),
    ("duplicate-tab", "tab.duplicate"),
    ("reopen-tab", "tab.reopen-closed"),
    ("split-right", "pane.split-right"),
    ("split-bottom", "pane.split-down"),
    ("close-pane", "pane.close"),
    ("pane-nav-next", "pane.focus-next"),
    ("copy", "terminal.copy"),
    ("paste", "terminal.paste"),
    ("search", "terminal.find"),
    ("clear", "terminal.clear"),
    ("select-all", "terminal.select-all"),
    ("command-selector", "view.command-palette"),
    ("toggle-fullscreen", "view.toggle-fullscreen"),
    ("zoom-in", "view.zoom-in"),
    ("zoom-out", "view.zoom-out"),
    ("reset-zoom", "view.zoom-reset"),
    ("settings", "settings.open"),
];

fn tervo_command(tabby_id: &str) -> Option<&'static str> {
    TABBY_HOTKEY_MAP.iter().find(|(tabby, _)| *tabby == tabby_id).map(|(_, tervo)| *tervo)
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn as_u16(v: Option<&Value>) -> Option<u16> {
    match v {
        Some(Value::Number(n)) => n.as_u64().and_then(|x| u16::try_from(x).ok()),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_string_seq(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_sequence)
        .map(|seq| seq.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn as_env(v: Option<&Value>) -> Vec<(String, String)> {
    let Some(map) = v.and_then(Value::as_mapping) else { return Vec::new() };
    map.iter()
        .filter_map(|(k, val)| {
            let key = k.as_str()?;
            let value = val.as_str().or_else(|| val.as_u64().map(|_| ""))?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// 解析 Tabby YAML 并合并进 `store` / `cfg`（调用方负责 `store.save()` /
/// `cfg.save()`；主题直接写入 `themes_dir`）。
pub fn import_yaml(
    text: &str,
    store: &mut ProfileStore,
    cfg: &mut Config,
    themes_dir: &Path,
) -> Result<ImportReport, String> {
    let root: Value = serde_yaml::from_str(text).map_err(|e| format!("YAML 解析失败: {e}"))?;
    if !root.is_mapping() {
        return Err("配置顶层不是映射".to_string());
    }
    // Tabby 在全新配置里可以没有 profiles（例如只调过快捷键/主题）；
    // 只有显式写了非列表的 profiles 才算坏配置。
    let profiles = root.get("profiles").and_then(Value::as_sequence);
    if root.get("profiles").is_some() && profiles.is_none() {
        return Err("配置里的 profiles 不是列表".to_string());
    }
    let empty = Vec::new();
    let profiles = profiles.unwrap_or(&empty);

    let mut report = ImportReport::default();
    for (idx, item) in profiles.iter().enumerate() {
        let raw_name = as_str(item.get("name")).unwrap_or("").to_string();
        let name = if raw_name.is_empty() { format!("tabby-{idx}") } else { raw_name };
        let ty = as_str(item.get("type")).unwrap_or("");
        let opts = item.get("options");

        let mut p = Profile { name: name.clone(), ..Profile::default() };
        match ty {
            "local" => {
                p.kind = "local".into();
                p.shell = as_str(opts.and_then(|o| o.get("shell"))).unwrap_or("").to_string();
                p.args = as_string_seq(opts.and_then(|o| o.get("args")));
                if let Some(cwd) = as_str(opts.and_then(|o| o.get("cwd"))) {
                    if cwd != "~" {
                        p.cwd = Some(cwd.to_string());
                    }
                }
                p.env = as_env(opts.and_then(|o| o.get("env")));
            },
            "ssh" => {
                p.kind = "ssh".into();
                p.host = as_str(opts.and_then(|o| o.get("host"))).unwrap_or("").to_string();
                p.port = as_u16(opts.and_then(|o| o.get("port"))).unwrap_or(22);
                p.user = as_str(opts.and_then(|o| o.get("user"))).unwrap_or("").to_string();
                p.auth = match as_str(opts.and_then(|o| o.get("auth"))) {
                    Some("key") => "key".into(),
                    Some("password") => "password".into(),
                    _ => "agent".into(),
                };
                p.key_path = as_str(opts.and_then(|o| o.get("privateKey")))
                    .or_else(|| as_str(opts.and_then(|o| o.get("privateKeyPath"))))
                    .unwrap_or("")
                    .to_string();
            },
            "serial" => {
                p.kind = "serial".into();
                p.serial_port = as_str(opts.and_then(|o| o.get("port")))
                    .or_else(|| as_str(opts.and_then(|o| o.get("path"))))
                    .unwrap_or("")
                    .to_string();
                p.baud = as_u16(opts.and_then(|o| o.get("baudRate")))
                    .map(u32::from)
                    .unwrap_or(115200);
            },
            "telnet" => {
                p.kind = "telnet".into();
                p.host = as_str(opts.and_then(|o| o.get("host"))).unwrap_or("").to_string();
                p.port = as_u16(opts.and_then(|o| o.get("port"))).unwrap_or(23);
            },
            other => {
                report.skipped.push((
                    name,
                    format!("不支持的 Profile 类型: {other}（只导入 local/ssh/serial/telnet）"),
                ));
                continue;
            },
        }

        // 重名不覆盖已有 Profile：加后缀，报告里计数。
        let base = p.name.clone();
        let mut candidate = base.clone();
        let mut n = 2;
        while store.profiles.iter().any(|x| x.name == candidate) {
            candidate = format!("{base} (tabby {n})");
            n += 1;
        }
        p.name = candidate.clone();
        match store.add(p) {
            Ok(_) => {
                report.added += 1;
                if candidate != base {
                    report.renamed += 1;
                }
            },
            Err(e) => report.skipped.push((base, e)),
        }
    }

    report.font_size = root
        .get("terminal")
        .and_then(|t| t.get("fontSize"))
        .and_then(Value::as_f64)
        .map(|v| v as f32);

    import_hotkeys(root.get("hotkeys"), cfg, &mut report);
    import_themes(root.get("terminal"), themes_dir, &mut report);
    report.not_imported = detect_not_imported(&root);
    Ok(report)
}

/// 扫描 Tabby 配置里**明确不支持**的内容，生成不兼容报告。
///
/// 只报告“用户可能以为导成功了”的东西：插件配置、UI 主题（插件 CSS）、
/// 自定义 CSS、密码类字段。终端配色（colorScheme/customColorSchemes）会被
/// 导入，不在此列。密码类字段只输出路径，绝不输出值。
fn detect_not_imported(root: &Value) -> Vec<NotImported> {
    let mut out = Vec::new();
    // 1) 插件配置：Tabby 的 plugins/pluginConfig 是各插件的自由 JSON，没有稳定 schema。
    for key in ["plugins", "pluginConfig", "pluginConfigs", "pluginsConfig"] {
        if let Some(v) = root.get(key) {
            if !v.is_null() {
                out.push(NotImported {
                    category: "plugin-config".into(),
                    detail: format!(
                        "{key}: Tabby 插件配置依赖插件自身 schema，Tervo 无对应插件模型"
                    ),
                });
            }
        }
    }
    // 2) UI 主题：只有主题名，实际 CSS 在 npm 插件里，无法从 config.yaml 取到。
    if let Some(theme) = root
        .get("appearance")
        .and_then(|a| a.get("theme"))
        .and_then(Value::as_str)
    {
        let theme = theme.trim();
        if !theme.is_empty() && !theme.eq_ignore_ascii_case("Follow the color scheme") {
            out.push(NotImported {
                category: "ui-theme-css".into(),
                detail: format!(
                    "appearance.theme = {theme:?}: UI 主题 CSS 由 Tabby 插件提供，未导入"
                ),
            });
        }
    }
    // 3) 自定义 CSS：只作用于 Tabby 的 Electron DOM，无法映射到终端网格。
    if let Some(css) = root
        .get("appearance")
        .and_then(|a| a.get("css"))
        .and_then(Value::as_str)
    {
        let chars = css.trim().chars().count();
        if chars > 0 {
            out.push(NotImported {
                category: "custom-css".into(),
                detail: format!("appearance.css: {chars} 字符自定义 CSS 仅作用于 Tabby UI，未导入"),
            });
        }
    }
    // 4) 密码/凭据字段：Tervo 只接受 Vault 加密保存，绝不会把明文搬进 profiles.toml。
    let mut secrets = Vec::new();
    collect_secret_paths(root, String::new(), &mut secrets);
    for path in secrets.into_iter().take(8) {
        out.push(NotImported {
            category: "credentials".into(),
            detail: format!("{path}: 凭据字段不导入（请在 Vault 中重新保存）"),
        });
    }
    out
}

/// 递归找密码类字段路径；只输出路径，不输出值。
fn collect_secret_paths(value: &Value, prefix: String, out: &mut Vec<String>) {
    match value {
        Value::Mapping(map) => {
            for (k, v) in map {
                let Some(key) = k.as_str() else { continue };
                let path = if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") };
                let lower = key.to_ascii_lowercase();
                let is_secret = lower.contains("password")
                    || lower.contains("secret")
                    || lower.contains("passphrase")
                    || matches!(lower.as_str(), "privatekeydata" | "keydata");
                if is_secret && !v.is_null() {
                    out.push(path.clone());
                    continue;
                }
                collect_secret_paths(v, path, out);
            }
        },
        Value::Sequence(seq) => {
            for (idx, v) in seq.iter().enumerate() {
                collect_secret_paths(v, format!("{prefix}[{idx}]"), out);
            }
        },
        _ => {},
    }
}

// ---------------------------------------------------------------- 快捷键

/// 把 Tabby 的 `hotkeys` 树展开成 (完整命令 id, 原始值)。
///
/// Tabby 自己用 [`getHotkeysConfigRecursive`] 做同样的事：对象分支拼成
/// `profile.<name>` / `profile-selectors.<id>` / `group-selectors.<id>`，
/// 数组（包括 multi-chord 的内层数组）保持为叶子。
fn flatten_hotkeys<'a>(prefix: &str, value: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    if let Some(map) = value.as_mapping() {
        for (key, child) in map {
            let Some(key) = key.as_str() else { continue };
            // Tabby 的 `__nonStructural` 是配置合并标记，不是命令。
            if key.starts_with("__") {
                continue;
            }
            let id = if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") };
            flatten_hotkeys(&id, child, out);
        }
    } else if !prefix.is_empty() {
        out.push((prefix.to_string(), value));
    }
}

fn import_hotkeys(value: Option<&Value>, cfg: &mut Config, report: &mut ImportReport) {
    let Some(value) = value else { return };
    if value.is_null() {
        return;
    }
    if !value.is_mapping() {
        report.hotkeys_skipped.push(("hotkeys".into(), "不是命令映射".into()));
        return;
    }

    let mut leaves = Vec::new();
    flatten_hotkeys("", value, &mut leaves);
    let registry = commands::builtin();
    for (tabby_id, raw) in leaves {
        let Some(tervo_id) = tervo_command(&tabby_id) else {
            report.hotkeys_unmapped.push(tabby_id);
            continue;
        };
        import_one_hotkey(&tabby_id, tervo_id, raw, &registry, cfg, report);
    }
}

fn import_one_hotkey(
    tabby_id: &str,
    tervo_id: &str,
    raw: &Value,
    registry: &Registry,
    cfg: &mut Config,
    report: &mut ImportReport,
) {
    // 已有自定义绑定绝不静默覆盖：报告跳过，保留用户现有选择。
    if let Some(existing) = cfg.hotkeys.iter().rev().find(|o| o.command == tervo_id) {
        report.hotkeys_skipped.push((
            tabby_id.to_string(),
            format!("Tervo 已有自定义绑定 {}（未覆盖）", existing.chord),
        ));
        return;
    }

    let alternatives: Vec<&Value> = match raw {
        Value::String(_) => vec![raw],
        Value::Sequence(seq) => seq.iter().collect(),
        Value::Null => Vec::new(),
        _ => {
            report
                .hotkeys_skipped
                .push((tabby_id.to_string(), "值类型不支持（只支持字符串/数组）".into()));
            return;
        },
    };
    if alternatives.is_empty() {
        report.hotkeys_skipped.push((
            tabby_id.to_string(),
            "空绑定（Tabby 中禁用），Tervo 无法表达“取消默认键”".into(),
        ));
        return;
    }

    let mut last_error: Option<String> = None;
    for (idx, alt) in alternatives.iter().enumerate() {
        let text = match alt {
            Value::String(s) => s,
            Value::Sequence(_) => {
                last_error = Some("多段组合键（multi-chord）暂不支持".into());
                continue;
            },
            _ => {
                last_error = Some("候选键不是字符串".into());
                continue;
            },
        };
        let chord = match tabby_chord(text) {
            Ok(chord) => chord,
            Err(e) => {
                last_error = Some(e);
                continue;
            },
        };
        if !chord.is_bindable() {
            last_error = Some(format!(
                "{text}: 需要 Ctrl/Alt/Meta 或 F1–F24（避免抢占终端输入）"
            ));
            continue;
        }
        let current_same = registry
            .effective_shortcut(tervo_id, &cfg.hotkeys)
            .and_then(|s| s.single_chord().cloned())
            .is_some_and(|current| current == chord);
        if current_same {
            report.hotkeys_unchanged += 1;
            report.hotkeys_extra_ignored += alternatives.len().saturating_sub(idx + 1);
            return;
        }
        match registry.validate_override(tervo_id, &chord.canonical(), &cfg.hotkeys) {
            Ok(chord) => {
                commands::set_override(&mut cfg.hotkeys, tervo_id, &chord);
                report.hotkeys_added += 1;
                report.hotkeys_extra_ignored += alternatives.len().saturating_sub(idx + 1);
                return;
            },
            Err(e) => last_error = Some(e),
        }
    }
    report
        .hotkeys_skipped
        .push((tabby_id.to_string(), last_error.unwrap_or_else(|| "没有可用绑定".into())));
}

/// Tabby chord → Tervo [`Chord`]。
///
/// Tabby 用 `-` 连接（`Ctrl-Shift-P`，字面 `-` 键写成 `Ctrl--`），macOS 默认
/// 配置里还混有 `Ctrl+⌘+F` 这种 `+` 写法，因此两种分隔符都接受；修饰键别名
/// （Super/⌘/Option…）与键名交给 Tervo 的解析器归一化。
fn tabby_chord(text: &str) -> Result<Chord, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("空快捷键".into());
    }
    let (mods, key) = if text.contains('-') {
        split_tabby_dash(text)?
    } else if text.contains('+') {
        split_tabby_plus(text)?
    } else {
        (Mods::default(), text.to_string())
    };
    if key.is_empty() {
        return Err(format!("缺少主键: {text}"));
    }
    let mut out = String::new();
    if mods.ctrl {
        out.push_str("ctrl+");
    }
    if mods.shift {
        out.push_str("shift+");
    }
    if mods.alt {
        out.push_str("alt+");
    }
    if mods.meta {
        out.push_str("meta+");
    }
    // Tervo 用 `+` 作分隔符；主键 `-` / `+` 走别名，避免歧义。
    match key.as_str() {
        "-" => out.push_str("minus"),
        "+" => out.push_str("plus"),
        _ => out.push_str(&key),
    }
    Chord::parse(&out).map_err(|e| format!("{text}: {e}"))
}

/// Tabby 标准写法：`Ctrl-Shift-P`、`F11`、`Ctrl--`（Ctrl + 减号）。
fn split_tabby_dash(text: &str) -> Result<(Mods, String), String> {
    let (body, key) = if let Some(core) = text.strip_suffix("--") {
        // 末尾的 `--` = 分隔符 + 字面减号主键；`-` 单独出现也算减号。
        (core, Some("-".to_string()))
    } else if text == "-" {
        ("", Some("-".to_string()))
    } else {
        (text, None)
    };
    let parts: Vec<&str> = if body.is_empty() { Vec::new() } else { body.split('-').collect() };
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("含空段: {text}"));
    }
    parse_tabby_parts(parts, key, text)
}

/// macOS 默认配置里的 `Ctrl+⌘+F`（Tabby 自身识别不了，导入时兼容）。
fn split_tabby_plus(text: &str) -> Result<(Mods, String), String> {
    let parts: Vec<&str> = text.split('+').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("含空段: {text}"));
    }
    parse_tabby_parts(parts, None, text)
}

fn parse_tabby_parts(
    parts: Vec<&str>,
    forced_key: Option<String>,
    original: &str,
) -> Result<(Mods, String), String> {
    let mut mods = Mods::default();
    let mut key: Option<String> = None;
    for (i, part) in parts.iter().enumerate() {
        if apply_tabby_modifier(part, &mut mods) {
            continue;
        }
        if key.is_some() {
            return Err(format!("一个组合里只能有一个主键: {original}"));
        }
        if i != parts.len() - 1 {
            return Err(format!("主键必须放在最后: {original}"));
        }
        key = Some((*part).to_string());
    }
    if forced_key.is_some() && key.is_some() {
        return Err(format!("主键重复: {original}"));
    }
    Ok((mods, key.or(forced_key).unwrap_or_default()))
}

/// Tabby 的修饰键别名（Linux `Super`、macOS `⌘`/`⌥`、Windows `Win`）。
fn apply_tabby_modifier(token: &str, mods: &mut Mods) -> bool {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => mods.ctrl = true,
        "shift" => mods.shift = true,
        "alt" | "option" | "⌥" => mods.alt = true,
        "meta" | "super" | "cmd" | "command" | "win" | "⌘" => mods.meta = true,
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------- 主题

#[derive(Debug, Clone, PartialEq, Eq)]
struct TabbyScheme {
    name: String,
    fg: u32,
    bg: u32,
    cursor: u32,
    ansi16: [u32; 16],
}

/// 写主题用的完整 Tervo TOML（结构与 `themes::RawTheme` 一致）。
#[derive(Serialize)]
struct ThemeToml<'a> {
    schema: u32,
    name: &'a str,
    variant: &'a str,
    foreground: String,
    background: String,
    cursor: String,
    ansi: [String; 16],
    ui: UiToml,
}

#[derive(Serialize)]
struct UiToml {
    bg: String,
    surface: String,
    raise: String,
    hover: String,
    border: String,
    text: String,
    subtext: String,
    muted: String,
    accent: String,
    #[serde(rename = "accent-text")]
    accent_text: String,
    danger: String,
}

fn hex(color: u32) -> String {
    format!("#{:06x}", color & 0x00ff_ffff)
}

fn theme_toml(scheme: &TabbyScheme) -> Result<String, String> {
    let spec = PaletteSpec {
        fg: scheme.fg,
        bg: scheme.bg,
        cursor: scheme.cursor,
        bright_fg: scheme.fg,
        dim_fg: scheme.fg,
        ansi16: scheme.ansi16,
    };
    let ui = UiColors::derive(&spec);
    let variant = if themes::is_dark_bg(spec.bg) { "dark" } else { "light" };
    let file = ThemeToml {
        schema: 1,
        name: &scheme.name,
        variant,
        foreground: hex(spec.fg),
        background: hex(spec.bg),
        cursor: hex(spec.cursor),
        ansi: std::array::from_fn(|i| hex(spec.ansi16[i])),
        ui: UiToml {
            bg: hex(ui.bg),
            surface: hex(ui.surface),
            raise: hex(ui.raise),
            hover: hex(ui.hover),
            border: hex(ui.border),
            text: hex(ui.text),
            subtext: hex(ui.subtext),
            muted: hex(ui.muted),
            accent: hex(ui.accent),
            accent_text: hex(ui.accent_text),
            danger: hex(ui.danger),
        },
    };
    toml::to_string_pretty(&file).map_err(|e| format!("主题 TOML 序列化失败: {e}"))
}

fn import_themes(terminal: Option<&Value>, dir: &Path, report: &mut ImportReport) {
    let Some(terminal) = terminal else { return };
    if terminal.is_null() {
        return;
    }
    if !terminal.is_mapping() {
        report.themes_skipped.push(("terminal".into(), "不是映射".into()));
        return;
    }

    let mut schemes: Vec<TabbyScheme> = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    // 当前正在用的深/浅色方案先收，再收用户保存的 customColorSchemes。
    for key in ["colorScheme", "lightColorScheme"] {
        let Some(raw) = terminal.get(key).filter(|v| !v.is_null()) else { continue };
        match parse_tabby_scheme(raw) {
            Ok(scheme) => schemes.push(scheme),
            Err(e) => errors.push((scheme_label(raw, key), e)),
        }
    }
    if let Some(customs) = terminal.get("customColorSchemes").filter(|v| !v.is_null()) {
        if let Some(seq) = customs.as_sequence() {
            for (i, raw) in seq.iter().enumerate() {
                match parse_tabby_scheme(raw) {
                    Ok(scheme) => schemes.push(scheme),
                    Err(e) => errors.push((scheme_label(raw, &format!("customColorSchemes[{i}]")), e)),
                }
            }
        } else {
            errors.push(("terminal.customColorSchemes".into(), "不是数组".into()));
        }
    }

    report.themes_skipped.append(&mut errors);
    if schemes.is_empty() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        report.themes_skipped.push((
            dir.display().to_string(),
            format!("无法创建主题目录: {e}"),
        ));
        return;
    }

    let mut seen = HashSet::new();
    for scheme in schemes {
        // 完全相同的配色只写一次（active colorScheme 常在 customColorSchemes 里重复）。
        let key = (
            scheme.name.clone(),
            scheme.fg,
            scheme.bg,
            scheme.cursor,
            scheme.ansi16,
        );
        if !seen.insert(key) {
            report
                .themes_skipped
                .push((scheme.name.clone(), "与前面的配色完全相同，已去重".into()));
            continue;
        }
        write_theme(dir, &scheme, report);
    }
}

fn write_theme(dir: &Path, scheme: &TabbyScheme, report: &mut ImportReport) {
    let Some(slug) = themes::slugify(&scheme.name) else {
        report.themes_skipped.push((scheme.name.clone(), "name 无法生成文件名".into()));
        return;
    };
    let body = match theme_toml(scheme) {
        Ok(body) => body,
        Err(e) => {
            report.themes_skipped.push((scheme.name.clone(), e));
            return;
        },
    };
    let base = format!("tabby-{slug}");
    let mut suffix = 1u32;
    loop {
        let file = if suffix == 1 {
            format!("{base}.toml")
        } else {
            format!("{base}-{suffix}.toml")
        };
        let path: PathBuf = dir.join(&file);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == body => {
                report.themes_skipped.push((
                    scheme.name.clone(),
                    format!("{file} 已存在且内容相同（未覆盖）"),
                ));
                return;
            },
            Ok(_) => {
                suffix += 1;
                if suffix > 999 {
                    report.themes_skipped.push((
                        scheme.name.clone(),
                        "同名文件过多，放弃写入".into(),
                    ));
                    return;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::write(&path, &body) {
                    Ok(()) => {
                        report.themes_added += 1;
                        if suffix > 1 {
                            report.themes_renamed += 1;
                        }
                    },
                    Err(e) => report
                        .themes_skipped
                        .push((scheme.name.clone(), format!("写入 {file} 失败: {e}"))),
                }
                return;
            },
            Err(e) => {
                report
                    .themes_skipped
                    .push((scheme.name.clone(), format!("检查 {file} 失败: {e}")));
                return;
            },
        }
    }
}

fn scheme_label(raw: &Value, fallback: &str) -> String {
    as_str(raw.get("name")).map(str::to_string).unwrap_or_else(|| fallback.to_string())
}

fn parse_tabby_scheme(raw: &Value) -> Result<TabbyScheme, String> {
    if !raw.is_mapping() {
        return Err("配色必须是映射对象".into());
    }
    let name = as_str(raw.get("name")).ok_or("缺少非空 name")?.to_string();
    let fg = parse_tabby_color(as_str(raw.get("foreground")).ok_or("缺少 foreground")?)
        .map_err(|e| format!("foreground: {e}"))?;
    let bg = parse_tabby_color(as_str(raw.get("background")).ok_or("缺少 background")?)
        .map_err(|e| format!("background: {e}"))?;
    let cursor = parse_tabby_color(as_str(raw.get("cursor")).ok_or("缺少 cursor")?)
        .map_err(|e| format!("cursor: {e}"))?;
    let colors = raw
        .get("colors")
        .and_then(Value::as_sequence)
        .ok_or("缺少 colors 数组")?;
    if colors.len() != 16 {
        return Err(format!("colors 必须 16 项（当前 {}）", colors.len()));
    }
    let mut ansi16 = [0u32; 16];
    for (i, item) in colors.iter().enumerate() {
        let text = item.as_str().ok_or_else(|| format!("colors[{i}] 不是字符串"))?;
        ansi16[i] = parse_tabby_color(text).map_err(|e| format!("colors[{i}]: {e}"))?;
    }
    Ok(TabbyScheme { name, fg, bg, cursor, ansi16 })
}

/// Tabby 配色里的颜色：`#RRGGBB` / `#RGB`；其它 CSS 颜色写法（rgba/named）不支持。
fn parse_tabby_color(text: &str) -> Result<u32, String> {
    let s = text.trim().trim_matches('"').trim_matches('\'').trim();
    let hex = s
        .strip_prefix('#')
        .ok_or_else(|| format!("颜色必须形如 #RRGGBB（当前 {s:?}）"))?;
    let expanded;
    let hex = if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        expanded = hex.chars().flat_map(|c| [c, c]).collect::<String>();
        expanded.as_str()
    } else {
        hex
    };
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("颜色必须形如 #RRGGBB（当前 {s:?}）"));
    }
    u32::from_str_radix(hex, 16).map_err(|e| format!("颜色解析失败 {s:?}: {e}"))
}

// ---------------------------------------------------------------- 导出

/// 导出成 Tabby 可读的 YAML（只含我们支持的 Profile 字段）。
pub fn export_yaml(store: &ProfileStore) -> String {
    let mut profiles = Vec::new();
    for p in &store.profiles {
        let mut m = Mapping::new();
        m.insert("name".into(), p.name.clone().into());
        m.insert("type".into(), p.kind.clone().into());
        let mut opts = Mapping::new();
        match p.kind.as_str() {
            "ssh" => {
                opts.insert("host".into(), p.host.clone().into());
                opts.insert("port".into(), (p.port as u64).into());
                opts.insert("user".into(), p.user.clone().into());
                opts.insert("auth".into(), p.auth.clone().into());
                if !p.key_path.is_empty() {
                    opts.insert("privateKey".into(), p.key_path.clone().into());
                }
            },
            "serial" => {
                opts.insert("port".into(), p.serial_port.clone().into());
                opts.insert("baudRate".into(), (p.baud as u64).into());
            },
            "telnet" => {
                opts.insert("host".into(), p.host.clone().into());
                opts.insert("port".into(), (p.port as u64).into());
            },
            _ => {
                opts.insert("shell".into(), p.shell.clone().into());
                opts.insert("args".into(), Value::Sequence(p.args.iter().map(|a| a.clone().into()).collect()));
                if let Some(cwd) = &p.cwd {
                    opts.insert("cwd".into(), cwd.clone().into());
                }
                if !p.env.is_empty() {
                    let mut env = Mapping::new();
                    for (k, v) in &p.env {
                        env.insert(k.clone().into(), v.clone().into());
                    }
                    opts.insert("env".into(), Value::Mapping(env));
                }
            },
        }
        m.insert("options".into(), Value::Mapping(opts));
        profiles.push(Value::Mapping(m));
    }
    let mut root = Mapping::new();
    root.insert("version".into(), 1u64.into());
    root.insert("profiles".into(), Value::Sequence(profiles));
    serde_yaml::to_string(&Value::Mapping(root)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tervo-tabby-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_profiles() -> &'static str {
        r#"
version: 1
profiles:
  - name: Local
    type: local
    options:
      shell: /bin/zsh
      args: ["-l"]
      cwd: /tmp
      env:
        FOO: bar
  - name: Server
    type: ssh
    options:
      host: example.com
      port: 2222
      user: yt
      auth: key
      privateKey: /home/user/.ssh/id_ed25519
  - name: Board
    type: serial
    options:
      port: /dev/ttyUSB0
      baudRate: 115200
  - name: BBS
    type: telnet
    options:
      host: 10.0.0.1
      port: 23
  - name: Weird
    type: kubernetes
    options: {}
terminal:
  fontSize: 16
"#
    }

    #[test]
    fn imports_all_supported_kinds_and_reports_unknown() {
        let dir = temp_dir("profiles");
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(sample_profiles(), &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.added, 4, "四个受支持类型都应导入: {r:?}");
        assert_eq!(r.skipped.len(), 1);
        assert!(r.skipped[0].1.contains("kubernetes"), "{:?}", r.skipped);
        assert_eq!(r.font_size, Some(16.0));

        let ssh = store.profiles.iter().find(|p| p.name == "Server").unwrap();
        assert_eq!(ssh.kind, "ssh");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.auth, "key");
        assert_eq!(ssh.key_path, "/home/user/.ssh/id_ed25519");

        let local = store.profiles.iter().find(|p| p.name == "Local").unwrap();
        assert_eq!(local.shell, "/bin/zsh");
        assert_eq!(local.args, vec!["-l"]);
        assert_eq!(local.cwd.as_deref(), Some("/tmp"));
        assert_eq!(local.env, vec![("FOO".to_string(), "bar".to_string())]);

        let serial = store.profiles.iter().find(|p| p.name == "Board").unwrap();
        assert_eq!(serial.serial_port, "/dev/ttyUSB0");
        assert_eq!(serial.baud, 115200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_names_are_suffixed() {
        let dir = temp_dir("dup");
        let mut store = ProfileStore::default();
        store.add(Profile { name: "Local".into(), ..Profile::default() }).unwrap();
        let mut cfg = Config::default();
        let r = import_yaml(sample_profiles(), &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.added, 4);
        assert_eq!(r.renamed, 1);
        assert!(store.profiles.iter().any(|p| p.name == "Local (tabby 2)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_import_roundtrip() {
        let dir = temp_dir("roundtrip");
        let mut store = ProfileStore::default();
        store
            .add(Profile {
                name: "S".into(),
                kind: "ssh".into(),
                host: "h".into(),
                port: 2022,
                user: "u".into(),
                auth: "key".into(),
                key_path: "/k".into(),
                ..Profile::default()
            })
            .unwrap();
        store
            .add(Profile {
                name: "L".into(),
                kind: "local".into(),
                shell: "/bin/sh".into(),
                args: vec!["-c".into(), "true".into()],
                cwd: Some("/".into()),
                ..Profile::default()
            })
            .unwrap();
        let yaml = export_yaml(&store);
        let mut back = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(&yaml, &mut back, &mut cfg, &dir).unwrap();
        assert_eq!(r.added, 2);
        assert_eq!(back.profiles.len(), 2);
        let ssh = back.profiles.iter().find(|p| p.name == "S").unwrap();
        assert_eq!((ssh.kind.as_str(), ssh.port, ssh.user.as_str()), ("ssh", 2022, "u"));
        let local = back.profiles.iter().find(|p| p.name == "L").unwrap();
        assert_eq!(local.args, vec!["-c".to_string(), "true".to_string()]);
        assert_eq!(local.cwd.as_deref(), Some("/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_yaml_and_non_list_profiles_are_error_but_empty_is_ok() {
        let dir = temp_dir("broken");
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        assert!(import_yaml("profiles: [", &mut store, &mut cfg, &dir).is_err());
        assert!(import_yaml("profiles: {}", &mut store, &mut cfg, &dir).is_err());
        assert!(import_yaml("- not\n- a map\n", &mut store, &mut cfg, &dir).is_err());
        // Tabby 全新配置可以只有 version/快捷键：Profile 为空不算错误。
        let r = import_yaml("version: 1", &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.added, 0);
        assert_eq!(r.hotkeys_added, 0);
        assert!(store.profiles.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tabby_command_map_targets_exist_and_is_unique() {
        let registry = commands::builtin();
        let mut seen_tabby = HashSet::new();
        let mut seen_tervo = HashSet::new();
        for (tabby, tervo) in TABBY_HOTKEY_MAP {
            assert!(seen_tabby.insert(*tabby), "重复 Tabby 命令: {tabby}");
            assert!(seen_tervo.insert(*tervo), "重复 Tervo 命令: {tervo}");
            assert!(registry.get(tervo).is_some(), "Tervo 命令不存在: {tervo}");
        }
    }

    #[test]
    fn tabby_chords_normalize_to_tervo_canonical() {
        let cases = [
            ("Ctrl-Shift-P", "ctrl+shift+p"),
            ("F11", "f11"),
            ("Ctrl--", "ctrl+-"),
            ("Ctrl-Shift--", "ctrl+shift+-"),
            ("Ctrl-=", "ctrl+="),
            ("Ctrl-Alt-[", "ctrl+alt+["),
            ("Alt-1", "alt+1"),
            ("Super-1", "meta+1"),
            ("⌘-C", "meta+c"),
            ("Ctrl+⌘+F", "ctrl+meta+f"),
            ("Ctrl-Shift-Insert", "ctrl+shift+insert"),
            ("Ctrl-PageUp", "ctrl+pageup"),
        ];
        for (tabby, expected) in cases {
            let chord = tabby_chord(tabby).unwrap_or_else(|e| panic!("{tabby}: {e}"));
            assert_eq!(chord.canonical(), expected, "input={tabby}");
            assert_eq!(Chord::parse(&chord.canonical()).unwrap(), chord);
        }
        assert!(tabby_chord("Ctrl-Shift").is_err(), "纯修饰键不是合法 chord");
        assert!(tabby_chord("Ctrl-WheelUp").is_err());
    }

    #[test]
    fn imports_mapped_hotkeys_and_reports_unmapped_and_invalid() {
        let dir = temp_dir("hotkeys");
        let text = r#"
profiles: []
hotkeys:
  copy: Ctrl-Shift-Y
  paste: Ctrl-Alt-V
  command-selector: Ctrl-Alt-P
  close-tab: Ctrl-NoSuchKey
  select-all: Ctrl-Shift
  clear: 42
  tab-1: Alt-1
  profile:
    Local: Ctrl-Alt-1
  profile-selectors:
    local: Ctrl-Alt-2
terminal: {}
"#;
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(text, &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.hotkeys_added, 3, "{r:?}");
        assert_eq!(cfg.hotkeys.len(), 3);
        assert!(cfg
            .hotkeys
            .iter()
            .any(|o| o.command == "terminal.copy" && o.chord == "ctrl+shift+y"));
        assert!(cfg
            .hotkeys
            .iter()
            .any(|o| o.command == "terminal.paste" && o.chord == "ctrl+alt+v"));
        assert!(cfg
            .hotkeys
            .iter()
            .any(|o| o.command == "view.command-palette" && o.chord == "ctrl+alt+p"));

        assert_eq!(r.hotkeys_skipped.len(), 3, "{:?}", r.hotkeys_skipped);
        assert!(r.hotkeys_skipped.iter().any(|(id, why)| id == "close-tab"
            && (why.contains("未知键名") || why.contains("Ctrl-NoSuchKey"))));
        assert!(r.hotkeys_skipped.iter().any(|(id, why)| id == "select-all"
            && why.contains("缺少主键")));
        assert!(r.hotkeys_skipped.iter().any(|(id, why)| id == "clear"
            && why.contains("值类型")));

        let mut unmapped = r.hotkeys_unmapped.clone();
        unmapped.sort();
        assert_eq!(
            unmapped,
            vec![
                "profile-selectors.local".to_string(),
                "profile.Local".to_string(),
                "tab-1".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hotkey_conflicts_empty_bindings_and_multi_chords_are_skipped() {
        let dir = temp_dir("hotkeys-conflict");
        let text = r#"
profiles: []
hotkeys:
  copy: Ctrl-Alt-J       # 先写入
  paste: Ctrl-Alt-J      # 与上一条冲突
  duplicate-tab: [[Ctrl-K, Ctrl-D]]  # multi-chord
  reset-zoom: []         # Tabby 里禁用
  toggle-fullscreen: F11 # 与 Tervo 默认相同
terminal: {}
"#;
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(text, &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.hotkeys_added, 1, "{r:?}");
        assert_eq!(r.hotkeys_unchanged, 1, "{r:?}");
        assert_eq!(r.hotkeys_skipped.len(), 3, "{:?}", r.hotkeys_skipped);
        assert!(r
            .hotkeys_skipped
            .iter()
            .any(|(id, why)| id == "paste" && why.contains("冲突")));
        assert!(r
            .hotkeys_skipped
            .iter()
            .any(|(id, why)| id == "duplicate-tab" && why.contains("多段组合键")));
        assert!(r
            .hotkeys_skipped
            .iter()
            .any(|(id, why)| id == "reset-zoom" && why.contains("空绑定")));
        assert_eq!(cfg.hotkeys.len(), 1);
        assert_eq!(cfg.hotkeys[0].command, "terminal.copy");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scheme_yaml(name: &str, fg: &str, bg: &str, cursor: &str) -> String {
        let colors: Vec<String> = (0..16).map(|i| format!("#{:02x}2233", i * 8)).collect();
        format!(
            "    name: {name}\n    foreground: '{fg}'\n    background: '{bg}'\n    cursor: '{cursor}'\n    colors:\n{}",
            colors.iter().map(|c| format!("      - '{c}'")).collect::<Vec<_>>().join("\n")
        )
    }

    #[test]
    fn imports_color_schemes_writes_theme_toml_and_suffixes_existing() {
        let dir = temp_dir("themes");
        // 已存在同名但内容不同的文件：必须加后缀，绝不覆盖。
        std::fs::write(dir.join("tabby-tabby-test-dark.toml"), "# keep me\n").unwrap();

        let dark = scheme_yaml("Tabby Test Dark", "#cacaca", "#171717", "#bbbbbb");
        let light = scheme_yaml("Tabby Test Light", "#4d4d4c", "#ffffff", "#4d4d4c");
        let text = format!(
            r#"
profiles: []
terminal:
  colorScheme:
{dark}
  lightColorScheme:
{light}
  customColorSchemes:
    - name: Tabby Test Dark
      foreground: '#cacaca'
      background: '#171717'
      cursor: '#bbbbbb'
      colors:
{colors}
    - name: Bad Color
      foreground: '#gggggg'
      background: '#171717'
      cursor: '#bbbbbb'
      colors:
{colors}
"#,
            colors = (0..16)
                .map(|i| format!("        - '#{:02x}2233'", i * 8))
                .collect::<Vec<_>>()
                .join("\n")
        );

        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(&text, &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.themes_added, 2, "{r:?}");
        assert_eq!(r.themes_renamed, 1, "{r:?}");
        assert!(dir.join("tabby-tabby-test-dark-2.toml").exists());
        assert!(dir.join("tabby-tabby-test-light.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("tabby-tabby-test-dark.toml")).unwrap(),
            "# keep me\n",
            "已有文件不能被覆盖"
        );
        assert!(r
            .themes_skipped
            .iter()
            .any(|(name, why)| name == "Tabby Test Dark" && why.contains("去重")));
        assert!(r
            .themes_skipped
            .iter()
            .any(|(name, why)| name == "Bad Color" && why.contains("foreground")));

        let text = std::fs::read_to_string(dir.join("tabby-tabby-test-dark-2.toml")).unwrap();
        assert!(text.contains("schema = 1"), "{text}");
        assert!(text.contains("name = \"Tabby Test Dark\""), "{text}");
        assert!(text.contains("variant = \"dark\""), "{text}");
        assert!(text.contains("[ui]"), "{text}");
        assert!(text.contains("ansi"), "{text}");

        // 用真实注册表再解析一遍，确认生成文件能被 Tervo 加载。
        let reg = themes::ThemeRegistry::load(&dir);
        assert_eq!(reg.custom_count(), 2, "{:?}", reg.errors);
        assert!(reg.get("custom:tabby-tabby-test-dark-2").is_some());
        assert!(reg.get("custom:tabby-tabby-test-light").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_color_schemes_are_skipped_with_reasons() {
        let dir = temp_dir("bad-themes");
        let text = r#"
profiles: []
terminal:
  colorScheme:
    name: One Color
    foreground: '#ffffff'
    background: '#111111'
    cursor: '#ffffff'
    colors: ['#000000']
  customColorSchemes:
    - name: Short Colors
      foreground: '#ffffff'
      background: '#111111'
      cursor: '#ffffff'
      colors: ['#000000', '#111111']
    - name: Named Color
      foreground: red
      background: '#111111'
      cursor: '#ffffff'
      colors:
        - '#000000'
        - '#111111'
        - '#222222'
        - '#333333'
        - '#444444'
        - '#555555'
        - '#666666'
        - '#777777'
        - '#888888'
        - '#999999'
        - '#aaaaaa'
        - '#bbbbbb'
        - '#cccccc'
        - '#dddddd'
        - '#eeeeee'
        - '#ffffff'
"#;
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let r = import_yaml(text, &mut store, &mut cfg, &dir).unwrap();
        assert_eq!(r.themes_added, 0, "{r:?}");
        assert_eq!(r.themes_skipped.len(), 3, "{:?}", r.themes_skipped);
        assert!(r
            .themes_skipped
            .iter()
            .any(|(name, why)| name == "One Color" && why.contains("colors 必须 16 项")));
        assert!(r
            .themes_skipped
            .iter()
            .any(|(name, why)| name == "Short Colors" && why.contains("colors 必须 16 项")));
        assert!(r
            .themes_skipped
            .iter()
            .any(|(name, why)| name == "Named Color" && why.contains("foreground")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn colors_accept_short_hex_and_respect_dark_light_variant() {
        assert_eq!(parse_tabby_color("#abc").unwrap(), 0x00aabbcc);
        assert_eq!(parse_tabby_color("#AABBCC").unwrap(), 0x00aabbcc);
        assert!(parse_tabby_color("rgba(0,0,0,1)").is_err());
        assert!(parse_tabby_color("#12345").is_err());

        let scheme = TabbyScheme {
            name: "Bright".into(),
            fg: 0x00000000,
            bg: 0x00ffffff,
            cursor: 0x00000000,
            ansi16: [0; 16],
        };
        assert!(theme_toml(&scheme).unwrap().contains("variant = \"light\""));
    }

    #[test]
    fn unsupported_tabby_sections_produce_explicit_not_imported_report() {
        let dir = temp_dir("not-imported");
        let mut store = ProfileStore::default();
        let mut cfg = Config::default();
        let yaml = r#"
version: 1
appearance:
  theme: Hype
  css: |
    .tab-bar { background: #000; }
plugins:
  - name: tabby-ssh
    config: { keepalive: 30 }
  - name: tabby-docker
profiles:
  - name: Local
    type: local
    options:
      shell: /bin/zsh
      password: hunter2-super-secret
terminal:
  fontSize: 15
"#;
        let r = import_yaml(yaml, &mut store, &mut cfg, &dir).unwrap();
        // 受支持的 Profile 仍然导入；不受支持的部分只进报告，不阻塞。
        assert_eq!(r.added, 1, "{r:?}");
        let cats: Vec<&str> = r.not_imported.iter().map(|i| i.category.as_str()).collect();
        assert!(cats.contains(&"plugin-config"), "{:?}", r.not_imported);
        assert!(cats.contains(&"ui-theme-css"), "{:?}", r.not_imported);
        assert!(cats.contains(&"custom-css"), "{:?}", r.not_imported);
        assert!(cats.contains(&"credentials"), "{:?}", r.not_imported);

        // 报告给用户看，但绝不能回显密码值。
        let joined = r
            .not_imported
            .iter()
            .map(|i| format!("{} / {}", i.category, i.detail))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("profiles[0].options.password"), "{joined}");
        assert!(!joined.contains("hunter2"), "报告不能包含密码明文: {joined}");
        assert!(joined.contains("Hype"), "UI 主题名可用于提示: {joined}");
        // 摘要区分中英文，且按类别去重。
        let zh = r.not_imported_summary(true);
        assert!(zh.contains("插件配置") && zh.contains("密码/凭据"), "{zh}");
        let en = r.not_imported_summary(false);
        assert!(en.contains("plugin configs") && en.contains("passwords"), "{en}");

        // 干净的 Tabby 配置不应出现“未导入”噪音。
        let mut s2 = ProfileStore::default();
        let mut c2 = Config::default();
        let clean = import_yaml(sample_profiles(), &mut s2, &mut c2, &dir).unwrap();
        assert!(clean.not_imported.is_empty(), "{:?}", clean.not_imported);
        assert!(clean.not_imported_summary(true).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
