// SPDX-License-Identifier: GPL-3.0-only
//! 命令注册表：所有用户可见操作都有稳定 ID。
//!
//! 菜单、快捷键、Command Palette、插件都从这张表取命令；UI 只负责展示与触发，
//! 不再各自硬编码字符串。快捷键解析/格式化、冲突检测、模糊搜索都在这层做，便于单测。
//!
//! 与 UI 解耦：这里不依赖 Slint，`Shortcut` 用独立的 `Mods` 表示修饰键。

#![allow(dead_code)] // 命令表先于 UI 接线落地：菜单/快捷键/插件随后接入

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 修饰键（与具体窗口系统/框架解耦）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

/// 一个按键组合（不含多段）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chord {
    pub mods: Mods,
    /// 归一化后的键名：小写字母/数字，或 `enter` / `esc` / `tab` / `space` / `f1`…
    pub key: String,
}

/// 快捷键：支持 1–2 段（Tabby 支持 multi-chord）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub chords: Vec<Chord>,
}

/// 命令分组，决定菜单/设置页里的归类与排序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    Workspace,
    Window,
    Pane,
    Terminal,
    View,
    Session,
    Settings,
    /// 外部进程插件声明的命令（ID 由 `plugin.<name>.<id>` 命名空间隔离）。
    Plugin,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Window => "Window",
            Self::Pane => "Pane",
            Self::Terminal => "Terminal",
            Self::View => "View",
            Self::Session => "Session",
            Self::Settings => "Settings",
            Self::Plugin => "Plugins",
        }
    }
}

/// 一条命令定义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// 稳定 ID（kebab-case），插件/配置引用它，不可重命名。
    pub id: &'static str,
    pub title: &'static str,
    pub category: Category,
    /// 默认快捷键（解析失败视为没有，启动时会报警）。
    pub default_shortcut: Option<&'static str>,
}

/// 快捷键冲突：同一个绑定落在多个命令上。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub shortcut: Shortcut,
    pub command_ids: Vec<&'static str>,
}

/// 用户快捷键覆盖（`config.toml` 里的 `[[hotkeys]]`）。
///
/// `chord` 统一存小写规范格式（`ctrl+shift+t` / `f12` / `ctrl+=`），见
/// [`Chord::canonical`]；加载时只做解析，非法项在生效/分发时被忽略。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyOverride {
    pub command: String,
    pub chord: String,
}

/// 某条命令当前实际生效的绑定（override 优先于默认）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveBinding {
    pub command_id: &'static str,
    pub shortcut: Shortcut,
    /// true = 来自用户配置，而不是命令表的默认绑定。
    pub custom: bool,
}

/// 命令表。内置命令在 [`builtin`] 里集中声明。
#[derive(Debug, Default)]
pub struct Registry {
    commands: Vec<Command>,
    by_id: HashMap<&'static str, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册命令；重复 ID 返回 Err（启动期就应炸出来，而不是运行时随机覆盖）。
    pub fn register(&mut self, command: Command) -> Result<(), &'static str> {
        if self.by_id.contains_key(command.id) {
            return Err(command.id);
        }
        self.by_id.insert(command.id, self.commands.len());
        self.commands.push(command);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Command> {
        self.by_id.get(id).map(|&i| &self.commands[i])
    }

    pub fn all(&self) -> &[Command] {
        &self.commands
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// 有默认快捷键的命令（解析成功才算）。
    pub fn bindings(&self) -> Vec<(Shortcut, &'static str)> {
        self.commands
            .iter()
            .filter_map(|c| c.default_shortcut.and_then(|s| Shortcut::parse(s).ok().map(|sc| (sc, c.id))))
            .collect()
    }

    /// 某条命令的 override；命令不在表里则返回 None。同一命令多条时以最后一条为准。
    pub fn override_for<'a>(
        &self,
        command_id: &str,
        overrides: &'a [HotkeyOverride],
    ) -> Option<&'a HotkeyOverride> {
        self.get(command_id)?;
        overrides.iter().rev().find(|o| o.command == command_id)
    }

    /// 某条命令当前生效的绑定：合法 override 优先，否则回退到默认。
    pub fn effective_shortcut(
        &self,
        command_id: &str,
        overrides: &[HotkeyOverride],
    ) -> Option<Shortcut> {
        let command = self.get(command_id)?;
        if let Some(chord) = self
            .override_for(command_id, overrides)
            .and_then(|o| Chord::parse(&o.chord).ok())
        {
            return Some(Shortcut { chords: vec![chord] });
        }
        command
            .default_shortcut
            .and_then(|s| Shortcut::parse(s).ok())
    }

    /// 全部命令当前生效的绑定（未知命令 / 非法 chord 的 override 被忽略）。
    pub fn effective_bindings(&self, overrides: &[HotkeyOverride]) -> Vec<EffectiveBinding> {
        self.commands
            .iter()
            .filter_map(|command| {
                let custom = self
                    .override_for(command.id, overrides)
                    .and_then(|o| Chord::parse(&o.chord).ok())
                    .map(|chord| Shortcut { chords: vec![chord] });
                let shortcut = custom
                    .clone()
                    .or_else(|| command.default_shortcut.and_then(|s| Shortcut::parse(s).ok()))?;
                Some(EffectiveBinding { command_id: command.id, shortcut, custom: custom.is_some() })
            })
            .collect()
    }

    /// 检测重复绑定（同一快捷键落在多个命令上）；包含用户 override。
    pub fn conflicts(&self) -> Vec<Conflict> {
        self.effective_conflicts(&[])
    }

    /// 与 [`Self::conflicts`] 相同，但把用户 override 计入生效绑定。
    pub fn effective_conflicts(&self, overrides: &[HotkeyOverride]) -> Vec<Conflict> {
        let mut grouped: Vec<Conflict> = Vec::new();
        for binding in self.effective_bindings(overrides) {
            if let Some(conflict) = grouped.iter_mut().find(|c| c.shortcut == binding.shortcut) {
                if !conflict.command_ids.contains(&binding.command_id) {
                    conflict.command_ids.push(binding.command_id);
                }
            } else {
                grouped.push(Conflict {
                    shortcut: binding.shortcut,
                    command_ids: vec![binding.command_id],
                });
            }
        }
        grouped.retain(|c| c.command_ids.len() > 1);
        grouped
    }

    /// 校验一条 override 能否保存；冲突 / 非法 chord / 未知命令都返回可展示的错误。
    pub fn validate_override(
        &self,
        command_id: &str,
        chord_text: &str,
        overrides: &[HotkeyOverride],
    ) -> Result<Chord, String> {
        self.get(command_id).ok_or_else(|| format!("未知命令: {command_id}"))?;
        let chord = Chord::parse(chord_text)?;
        if !chord.is_bindable() {
            return Err("快捷键需要 Ctrl/Alt/Meta，或使用 F1–F24".to_string());
        }
        let wanted = Shortcut { chords: vec![chord.clone()] };
        for binding in self.effective_bindings(overrides) {
            // 命令自己的旧绑定会被这次 override 替换，不算冲突。
            if binding.command_id == command_id || binding.shortcut != wanted {
                continue;
            }
            let title = self.get(binding.command_id).map(|c| c.title).unwrap_or(binding.command_id);
            return Err(format!("冲突：{title} 已使用 {}", wanted));
        }
        Ok(chord)
    }

    /// 某个单段 chord 是否已被命令的**默认**绑定占用（Profile 热键冲突提示用）。
    ///
    /// 只看 `default_shortcut`，不读用户 override —— 冲突提示是给用户看的，
    /// 而 override 本身就是用户自己的选择。
    pub fn default_command_for_chord(&self, chord: &Chord) -> Option<&'static str> {
        self.all().iter().find_map(|command| {
            let default = command.default_shortcut.and_then(|s| Shortcut::parse(s).ok())?;
            (default.chords.len() == 1 && default.chords[0] == *chord).then_some(command.id)
        })
    }

    /// 分发用：找到当前按键 chord 命中的 override 命令（只认用户覆盖，按配置倒序）。
    ///
    /// 未知命令、非法 chord、无修饰的普通字符都跳过；普通字符不允许抢走终端输入。
    pub fn override_command_for_chord(
        &self,
        chord: &Chord,
        overrides: &[HotkeyOverride],
    ) -> Option<&'static str> {
        if !chord.is_bindable() {
            return None;
        }
        for over in overrides.iter().rev() {
            let Some(command) = self.get(&over.command) else { continue };
            if Chord::parse(&over.chord).is_ok_and(|bound| bound == *chord) {
                return Some(command.id);
            }
        }
        None
    }

    /// 分发用：这个 chord 是否是某条命令**已被改走**的旧默认键。
    ///
    /// 命中时调用方应吞掉按键，避免“改键后旧快捷键仍在触发”的重复行为。
    pub fn suppressed_default(
        &self,
        chord: &Chord,
        overrides: &[HotkeyOverride],
    ) -> Option<&'static str> {
        self.commands.iter().find_map(|command| {
            let default = command.default_shortcut.and_then(|s| Shortcut::parse(s).ok())?;
            if default.chords.len() != 1 || default.chords[0] != *chord {
                return None;
            }
            let bound = Chord::parse(&self.override_for(command.id, overrides)?.chord).ok()?;
            (bound != *chord && bound.is_bindable()).then_some(command.id)
        })
    }

    /// 插件命令的默认快捷键：仅在输入层的内置硬编码快捷键都未命中时调用。
    ///
    /// 只认 `plugin.` 命名空间，内置命令永远不会从这里触发；插件键冲突时先注册者优先。
    pub fn plugin_default_for_chord(&self, chord: &Chord) -> Option<&'static str> {
        self.commands.iter().find_map(|command| {
            if !command.id.starts_with("plugin.") {
                return None;
            }
            let shortcut = command.default_shortcut.and_then(|s| Shortcut::parse(s).ok())?;
            (shortcut.chords.len() == 1 && shortcut.chords[0] == *chord).then_some(command.id)
        })
    }

    /// Command Palette 的模糊搜索：子序列匹配标题或 ID，连续命中与词首命中加权。
    pub fn search(&self, query: &str) -> Vec<&Command> {
        let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
        if q.is_empty() {
            return self.commands.iter().collect();
        }
        let mut hits: Vec<(i32, &Command)> = self
            .commands
            .iter()
            .filter_map(|c| fuzzy_score(c.title, c.id, &q).map(|s| (s, c)))
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.title.cmp(b.1.title)));
        hits.into_iter().map(|(_, c)| c).collect()
    }
}

/// 返回得分（越大越好）；不匹配返回 None。
fn fuzzy_score(title: &str, id: &str, query: &[char]) -> Option<i32> {
    let title_l = title.to_lowercase();
    let mut score = score_in(&title_l, query)?;
    // ID 命中作为补充（例如搜 "spl" 也能找到 "pane.split-right"）
    if let Some(s) = score_in(id, query) {
        score = score.max(s / 2);
    }
    // 词首命中加权：查询首字符落在词首
    if title_l.starts_with(query[0]) {
        score += 30;
    }
    Some(score)
}

fn score_in(text: &str, query: &[char]) -> Option<i32> {
    let chars: Vec<char> = text.chars().collect();
    let mut qi = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;
    for (i, c) in chars.iter().enumerate() {
        if qi < query.len() && *c == query[qi] {
            score += 10;
            if last == Some(i.wrapping_sub(1)) {
                score += 8; // 连续命中
            }
            if i == 0 || !chars[i - 1].is_alphanumeric() {
                score += 6; // 词首
            }
            last = Some(i);
            qi += 1;
        }
    }
    (qi == query.len()).then_some(score)
}

impl Shortcut {
    /// 解析 `ctrl+shift+p` 或 `ctrl+k ctrl+t`（空格分隔多段、`+` 连接修饰键）。
    pub fn parse(text: &str) -> Result<Self, String> {
        let chords: Result<Vec<Chord>, String> =
            text.split_whitespace().map(Chord::parse).collect();
        let chords = chords?;
        if chords.is_empty() {
            return Err("空快捷键".into());
        }
        if chords.len() > 2 {
            return Err(format!("最多支持两段组合键: {text}"));
        }
        Ok(Self { chords })
    }

    /// 单段快捷键返回该 chord；多段（尚未支持录制/override）返回 None。
    pub fn single_chord(&self) -> Option<&Chord> {
        match self.chords.as_slice() {
            [chord] => Some(chord),
            _ => None,
        }
    }

    /// 配置/比较用的规范小写格式：`ctrl+shift+t`、`ctrl+k ctrl+t`。
    pub fn canonical(&self) -> String {
        self.chords.iter().map(Chord::canonical).collect::<Vec<_>>().join(" ")
    }
}

impl std::fmt::Display for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, chord) in self.chords.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{chord}")?;
        }
        Ok(())
    }
}

impl Chord {
    /// 解析单个组合键：修饰键 `+` 主键。大小写不敏感，接受常见别名。
    ///
    /// 主键 `+` 写成 `ctrl++` / `ctrl+shift++`（末尾两个 `+`），避免与分隔符混淆。
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("空快捷键".into());
        }
        let (body, plus_key) = if let Some(core) = text.strip_suffix("++") {
            if core.ends_with('+') {
                return Err(format!("非法组合键: {text}"));
            }
            (core, true)
        } else if text.ends_with('+') {
            return Err(format!("缺少主键: {text}"));
        } else {
            (text, false)
        };

        let mut mods = Mods::default();
        let mut key: Option<String> = None;
        for part in body.split('+') {
            let p = part.trim().to_lowercase();
            match p.as_str() {
                "" => continue,
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "option" => mods.alt = true,
                "meta" | "super" | "cmd" | "win" => mods.meta = true,
                other => {
                    if key.is_some() {
                        return Err(format!("一个组合里只能有一个主键: {text}"));
                    }
                    key = Some(normalize_key(other)?);
                },
            }
        }
        let key = match (key, plus_key) {
            (Some(_), true) => return Err(format!("一个组合里只能有一个主键: {text}")),
            (Some(key), false) => key,
            (None, true) => "+".to_string(),
            (None, false) => return Err(format!("缺少主键: {text}")),
        };
        Ok(Chord { mods, key })
    }

    /// 配置/比较用的规范小写格式。
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        if self.mods.ctrl {
            out.push_str("ctrl+");
        }
        if self.mods.shift {
            out.push_str("shift+");
        }
        if self.mods.alt {
            out.push_str("alt+");
        }
        if self.mods.meta {
            out.push_str("meta+");
        }
        out.push_str(&self.key);
        out
    }

    /// 是否适合作为全局快捷键：至少一个 Ctrl/Alt/Meta，或功能键。
    ///
    /// 单独 Shift / 普通字符不参与分发，避免抢占终端打字。
    pub fn is_bindable(&self) -> bool {
        self.mods.ctrl || self.mods.alt || self.mods.meta || is_function_key(&self.key)
    }
}

impl std::fmt::Display for Chord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.mods.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.mods.shift {
            f.write_str("Shift+")?;
        }
        if self.mods.alt {
            f.write_str("Alt+")?;
        }
        if self.mods.meta {
            f.write_str("Meta+")?;
        }
        // 单字符键显示大写（Ctrl+T 而不是 Ctrl+t）；tab/f11/right 等保持原样
        if self.key.chars().count() == 1 {
            f.write_str(&self.key.to_uppercase())
        } else {
            f.write_str(&self.key)
        }
    }
}

/// 键名归一化：小写、别名展开，并拒绝未知键名（保证非法 chord 能返错）。
fn normalize_key(key: &str) -> Result<String, String> {
    let mapped = match key {
        "esc" => "escape",
        "return" => "enter",
        "del" => "delete",
        "ins" => "insert",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "spacebar" => "space",
        "plus" => "+",
        "minus" => "-",
        "equal" => "=",
        other => other,
    };
    let lowered = mapped.to_lowercase();
    let named = matches!(
        lowered.as_str(),
        "enter"
            | "escape"
            | "tab"
            | "space"
            | "backspace"
            | "delete"
            | "insert"
            | "home"
            | "end"
            | "pageup"
            | "pagedown"
            | "up"
            | "down"
            | "left"
            | "right"
    );
    let single = lowered.chars().count() == 1 && lowered.chars().next().is_some_and(|c| c.is_ascii_graphic());
    if named || single || is_function_key(&lowered) {
        Ok(lowered)
    } else {
        Err(format!("未知键名: {key}"))
    }
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n))
}

impl HotkeyOverride {
    /// 新建一条 override，chord 自动转成规范小写格式。
    pub fn new(command: &str, chord: &Chord) -> Self {
        Self { command: command.to_string(), chord: chord.canonical() }
    }
}

/// 写入/替换某命令的 override（同一命令只保留一条，规范格式）。
pub fn set_override(overrides: &mut Vec<HotkeyOverride>, command: &str, chord: &Chord) {
    overrides.retain(|o| o.command != command);
    overrides.push(HotkeyOverride::new(command, chord));
}

/// 清除某命令的 override；返回是否真的删掉了。
pub fn clear_override(overrides: &mut Vec<HotkeyOverride>, command: &str) -> bool {
    let before = overrides.len();
    overrides.retain(|o| o.command != command);
    overrides.len() != before
}

/// 内置命令集：一个用户可见操作对应一条。
pub fn builtin() -> Registry {
    let mut r = Registry::new();
    let cmds = [
        ("window.new", "New Window", Category::Window, Some("ctrl+shift+n")),
        ("tab.new", "New Tab", Category::Workspace, Some("ctrl+t")),
        ("tab.search", "Search Tabs", Category::Workspace, Some("ctrl+shift+a")),
        ("tab.close", "Close Tab", Category::Workspace, Some("ctrl+w")),
        ("tab.next", "Next Tab", Category::Workspace, Some("ctrl+tab")),
        ("tab.prev", "Previous Tab", Category::Workspace, Some("ctrl+shift+tab")),
        ("tab.last", "Last Used Tab", Category::Workspace, Some("ctrl+shift+l")),
        ("tab.duplicate", "Duplicate Tab", Category::Workspace, None),
        ("tab.reopen-closed", "Reopen Closed Tab", Category::Workspace, Some("ctrl+shift+t")),
        ("tab.rename", "Rename Tab", Category::Workspace, None),
        ("tab.rename-undo", "Undo Tab Rename", Category::Workspace, Some("ctrl+shift+z")),
        ("pane.split-right", "Split Right", Category::Pane, Some("ctrl+shift+e")),
        ("pane.split-down", "Split Down", Category::Pane, Some("ctrl+shift+d")),
        ("pane.close", "Close Pane", Category::Pane, Some("ctrl+shift+w")),
        ("pane.undo-swap", "Undo Pane Move", Category::Pane, Some("ctrl+alt+z")),
        ("pane.focus-next", "Focus Next Pane", Category::Pane, Some("ctrl+shift+right")),
        ("terminal.copy", "Copy", Category::Terminal, Some("ctrl+shift+c")),
        ("terminal.paste", "Paste", Category::Terminal, Some("ctrl+shift+v")),
        ("terminal.find", "Find", Category::Terminal, Some("ctrl+f")),
        ("terminal.clear", "Clear Screen", Category::Terminal, None),
        ("terminal.select-all", "Select All", Category::Terminal, None),
        ("terminal.toggle-read-only", "Toggle Read Only", Category::Terminal, None),
        ("terminal.reset", "Reset Terminal", Category::Terminal, None),
        ("terminal.reset-and-clear", "Reset and Clear", Category::Terminal, None),
        ("terminal.save-contents", "Save Contents…", Category::Terminal, None),
        ("view.command-palette", "Command Palette", Category::View, Some("ctrl+shift+p")),
        ("view.toggle-fullscreen", "Toggle Fullscreen", Category::View, Some("f11")),
        ("view.sidebar", "Toggle Sidebar", Category::View, Some("ctrl+shift+b")),
        ("view.toggle-quake", "Toggle Quake Mode", Category::View, None),
        ("view.scroll-top", "Scroll to Top", Category::View, Some("ctrl+shift+home")),
        ("view.scroll-bottom", "Scroll to Bottom", Category::View, Some("ctrl+shift+end")),
        ("workspace.save", "Save Layout as Workspace", Category::View, None),
        ("workspace.list", "List Workspaces", Category::View, None),
        ("workspace.open", "Open Last Workspace", Category::View, None),
        ("view.zoom-in", "Zoom In", Category::View, Some("ctrl+plus")),
        ("view.zoom-out", "Zoom Out", Category::View, Some("ctrl+minus")),
        ("view.zoom-reset", "Reset Zoom", Category::View, Some("ctrl+0")),
        ("session.restart", "Restart Session", Category::Session, None),
        ("session.reattach", "Reattach Background Session", Category::Session, None),
        ("settings.open", "Open Settings", Category::Settings, None),
    ];
    for (id, title, category, shortcut) in cmds {
        r.register(Command { id, title, category, default_shortcut: shortcut })
            .expect("内置命令 ID 不能重复");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_is_consistent() {
        let r = builtin();
        assert!(r.len() >= 20);
        // ID 唯一、非空、kebab-case
        for c in r.all() {
            assert!(!c.id.is_empty());
            assert!(c.id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-' || ch == '.' || ch.is_ascii_digit()), "{}", c.id);
            assert_eq!(r.get(c.id).map(|x| x.id), Some(c.id));
        }
        // 默认快捷键必须都能解析
        for c in r.all() {
            if let Some(s) = c.default_shortcut {
                Shortcut::parse(s).unwrap_or_else(|e| panic!("{} 的快捷键 {s} 解析失败: {e}", c.id));
            }
        }
    }

    #[test]
    fn default_command_for_chord_finds_known_bindings() {
        let r = builtin();
        let chord = Chord::parse("ctrl+shift+e").unwrap();
        assert_eq!(r.default_command_for_chord(&chord), Some("pane.split-right"));
        // 无修饰普通字符不该命中任何命令（也拿不到终端之外的含义）
        let plain = Chord::parse("e").unwrap();
        assert_eq!(r.default_command_for_chord(&plain), None);
    }

    #[test]
    fn sidebar_command_is_registered_with_bindable_default() {
        let r = builtin();
        let cmd = r.get("view.sidebar").expect("view.sidebar 必须注册");
        assert_eq!(cmd.title, "Toggle Sidebar");
        assert_eq!(cmd.category, Category::View);
        let chord = Chord::parse("ctrl+shift+b").unwrap();
        // 默认键能被兜底分发解析到，且没有别的命令抢同一个键
        assert_eq!(r.default_command_for_chord(&chord), Some("view.sidebar"));
        let hits: Vec<_> = r
            .all()
            .iter()
            .filter(|c| c.default_shortcut == Some("ctrl+shift+b"))
            .map(|c| c.id)
            .collect();
        assert_eq!(hits, vec!["view.sidebar"]);
    }

    #[test]
    fn window_new_is_registered_with_default_shortcut() {
        let r = builtin();
        let cmd = r.get("window.new").expect("window.new 必须注册");
        assert_eq!(cmd.title, "New Window");
        assert_eq!(cmd.category, Category::Window);
        assert_eq!(cmd.category.label(), "Window");
        let sc = Shortcut::parse(cmd.default_shortcut.expect("默认快捷键")).unwrap();
        assert!(sc.chords[0].mods.ctrl && sc.chords[0].mods.shift);
        assert_eq!(sc.chords[0].key, "n");
        assert!(r.conflicts().is_empty());
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let mut r = Registry::new();
        r.register(Command { id: "a.b", title: "A", category: Category::View, default_shortcut: None }).unwrap();
        assert!(r.register(Command { id: "a.b", title: "B", category: Category::View, default_shortcut: None }).is_err());
    }

    #[test]
    fn shortcut_parse_and_format_roundtrip() {
        let s = Shortcut::parse("ctrl+shift+p").unwrap();
        assert_eq!(s.chords.len(), 1);
        assert!(s.chords[0].mods.ctrl && s.chords[0].mods.shift);
        assert_eq!(s.chords[0].key, "p");
        assert_eq!(s.to_string(), "Ctrl+Shift+P");
        assert_eq!(s.canonical(), "ctrl+shift+p");

        let multi = Shortcut::parse("ctrl+k ctrl+t").unwrap();
        assert_eq!(multi.chords.len(), 2);
        assert_eq!(multi.canonical(), "ctrl+k ctrl+t");
        assert_eq!(multi.single_chord(), None);
        assert_eq!(s.single_chord().map(Chord::canonical).as_deref(), Some("ctrl+shift+p"));

        assert!(Shortcut::parse("ctrl+").is_err());
        assert!(Shortcut::parse("ctrl+a+b").is_err());
        assert!(Shortcut::parse("ctrl+a ctrl+b ctrl+c").is_err());
        assert!(Shortcut::parse("").is_err());
        // 别名归一化
        assert_eq!(Shortcut::parse("esc").unwrap().chords[0].key, "escape");
        assert_eq!(Shortcut::parse("Return").unwrap().chords[0].key, "enter");
        assert_eq!(Shortcut::parse("Del").unwrap().chords[0].key, "delete");
    }

    #[test]
    fn chord_parse_canonical_and_invalid_input() {
        for (input, canonical) in [
            ("ctrl+shift+t", "ctrl+shift+t"),
            ("Ctrl + Shift + T", "ctrl+shift+t"),
            ("f12", "f12"),
            ("F12", "f12"),
            ("ctrl+=", "ctrl+="),
            ("ctrl+plus", "ctrl++"),
            ("ctrl+shift++", "ctrl+shift++"),
            ("ctrl+minus", "ctrl+-"),
            ("PgUp", "pageup"),
            ("alt+space", "alt+space"),
            ("meta+enter", "meta+enter"),
        ] {
            let chord = Chord::parse(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(chord.canonical(), canonical, "input={input}");
            // 规范格式必须能解析回同一个 chord
            assert_eq!(Chord::parse(&chord.canonical()).unwrap(), chord, "input={input}");
        }

        for bad in [
            "",
            "ctrl+",
            "shift",
            "ctrl+a+b",
            "ctrl+ctrl",
            "ctrl+banana",
            "ctrl+",
            "ctrl+++",
            "ctrl+中",
        ] {
            assert!(Chord::parse(bad).is_err(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn chord_display_and_bindability() {
        assert_eq!(Chord::parse("ctrl+shift+t").unwrap().to_string(), "Ctrl+Shift+T");
        assert_eq!(Chord::parse("f12").unwrap().to_string(), "f12");
        // 普通字符不能当全局快捷键；F 键可以
        assert!(!Chord::parse("a").unwrap().is_bindable());
        assert!(!Chord::parse("shift+a").unwrap().is_bindable());
        assert!(Chord::parse("ctrl+a").unwrap().is_bindable());
        assert!(Chord::parse("alt+a").unwrap().is_bindable());
        assert!(Chord::parse("f12").unwrap().is_bindable());
    }

    fn test_registry() -> Registry {
        let mut r = Registry::new();
        r.register(Command {
            id: "x.one",
            title: "One",
            category: Category::View,
            default_shortcut: Some("ctrl+1"),
        })
        .unwrap();
        r.register(Command {
            id: "x.two",
            title: "Two",
            category: Category::View,
            default_shortcut: Some("ctrl+2"),
        })
        .unwrap();
        r.register(Command {
            id: "x.three",
            title: "Three",
            category: Category::View,
            default_shortcut: None,
        })
        .unwrap();
        r
    }

    #[test]
    fn override_lookup_and_effective_shortcut() {
        let r = test_registry();
        let mut overrides = Vec::new();
        let chord = Chord::parse("ctrl+shift+y").unwrap();
        set_override(&mut overrides, "x.one", &chord);

        assert_eq!(
            r.override_for("x.one", &overrides).map(|o| o.chord.as_str()),
            Some("ctrl+shift+y")
        );
        assert_eq!(
            r.effective_shortcut("x.one", &overrides).unwrap().canonical(),
            "ctrl+shift+y"
        );
        // 未覆盖的命令仍用默认；无默认仍为 None
        assert_eq!(r.effective_shortcut("x.two", &overrides).unwrap().canonical(), "ctrl+2");
        assert!(r.effective_shortcut("x.three", &overrides).is_none());
        // 未知命令的配置被忽略
        assert!(r.override_for("nope", &overrides).is_none());
        assert!(r.effective_shortcut("nope", &overrides).is_none());
        assert!(!overrides.iter().any(|o| o.command == "nope"));

        // 非法 chord 的 override 回退到默认
        overrides.push(HotkeyOverride { command: "x.two".into(), chord: "not-a-chord+".into() });
        assert_eq!(r.effective_shortcut("x.two", &overrides).unwrap().canonical(), "ctrl+2");

        // set 会去重；clear 返回是否删掉
        set_override(&mut overrides, "x.one", &Chord::parse("ctrl+3").unwrap());
        assert_eq!(overrides.iter().filter(|o| o.command == "x.one").count(), 1);
        assert!(clear_override(&mut overrides, "x.one"));
        assert!(!clear_override(&mut overrides, "x.one"));
        assert_eq!(r.effective_shortcut("x.one", &overrides).unwrap().canonical(), "ctrl+1");
    }

    #[test]
    fn override_conflict_detection_and_validation() {
        let r = test_registry();
        let mut overrides = Vec::new();

        // 与别的命令默认键冲突
        let err = r.validate_override("x.one", "ctrl+2", &overrides).unwrap_err();
        assert!(err.contains("Two"), "{err}");
        // 未知命令 / 非法 chord / 无修饰普通字符
        assert!(r.validate_override("nope", "ctrl+9", &overrides).is_err());
        assert!(r.validate_override("x.one", "ctrl+banana", &overrides).is_err());
        assert!(r.validate_override("x.one", "a", &overrides).is_err());
        // 改自己的键不算冲突
        let ok = r.validate_override("x.one", "ctrl+shift+y", &overrides).unwrap();
        assert_eq!(ok.canonical(), "ctrl+shift+y");
        // 填进配置后生效且无冲突
        set_override(&mut overrides, "x.one", &ok);
        assert!(r.effective_conflicts(&overrides).is_empty());

        // 把 x.two 也改到同一个键 → 冲突
        overrides.push(HotkeyOverride { command: "x.two".into(), chord: "ctrl+shift+y".into() });
        let conflicts = r.effective_conflicts(&overrides);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].command_ids, vec!["x.one", "x.two"]);
        assert!(r.validate_override("x.two", "ctrl+shift+y", &overrides).is_err());

        // 清除覆盖后冲突消失
        clear_override(&mut overrides, "x.two");
        assert!(r.effective_conflicts(&overrides).is_empty());
    }

    #[test]
    fn conflict_detection() {
        let mut r = Registry::new();
        r.register(Command { id: "x.one", title: "One", category: Category::View, default_shortcut: Some("ctrl+shift+x") }).unwrap();
        r.register(Command { id: "x.two", title: "Two", category: Category::View, default_shortcut: Some("ctrl+shift+x") }).unwrap();
        let conflicts = r.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].command_ids, vec!["x.one", "x.two"]);

        // 内置命令不允许有冲突
        assert!(builtin().conflicts().is_empty());
    }

    #[test]
    fn dispatch_helpers_ignore_unknown_and_plain_keys() {
        let r = test_registry();
        let mut overrides = Vec::new();
        set_override(&mut overrides, "x.one", &Chord::parse("ctrl+shift+y").unwrap());

        let hit = Chord::parse("ctrl+shift+y").unwrap();
        assert_eq!(r.override_command_for_chord(&hit, &overrides), Some("x.one"));
        // 没有 override 的默认键不算 override 命中
        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+2").unwrap(), &overrides),
            None
        );
        // 未知命令 / 非法 / 无修饰普通字符都不参与分发
        overrides.push(HotkeyOverride { command: "nope".into(), chord: "ctrl+shift+n".into() });
        overrides.push(HotkeyOverride { command: "x.two".into(), chord: "oops".into() });
        overrides.push(HotkeyOverride { command: "x.three".into(), chord: "a".into() });
        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+shift+n").unwrap(), &overrides),
            None
        );
        assert_eq!(r.override_command_for_chord(&Chord::parse("a").unwrap(), &overrides), None);
    }

    #[test]
    fn stale_default_is_suppressed_after_rebind() {
        let r = test_registry();
        let mut overrides = Vec::new();
        set_override(&mut overrides, "x.one", &Chord::parse("ctrl+shift+y").unwrap());

        // 旧默认键被改走 → 调用方吞掉
        let old = Chord::parse("ctrl+1").unwrap();
        assert_eq!(r.suppressed_default(&old, &overrides), Some("x.one"));
        // 新键 / 未改命令的默认键都不吞
        assert_eq!(r.suppressed_default(&Chord::parse("ctrl+shift+y").unwrap(), &overrides), None);
        assert_eq!(r.suppressed_default(&Chord::parse("ctrl+2").unwrap(), &overrides), None);
        // 覆盖成同一个键：仍按默认路径走，不算“被改走”
        let mut same = Vec::new();
        set_override(&mut same, "x.one", &Chord::parse("ctrl+1").unwrap());
        assert_eq!(r.suppressed_default(&old, &same), None);
    }

    #[test]
    fn builtin_override_end_to_end() {
        let r = builtin();
        // 无 override 时默认键不参与“override 命中”（仍走原硬编码路径）
        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+t").unwrap(), &[]),
            None
        );
        assert!(r.suppressed_default(&Chord::parse("ctrl+t").unwrap(), &[]).is_none());

        let mut overrides = Vec::new();
        let chord = r.validate_override("tab.new", "ctrl+alt+n", &overrides).unwrap();
        set_override(&mut overrides, "tab.new", &chord);

        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+alt+n").unwrap(), &overrides),
            Some("tab.new")
        );
        // 旧默认键被吞掉，避免 Ctrl+T 仍然新开 Tab
        assert_eq!(
            r.suppressed_default(&Chord::parse("ctrl+t").unwrap(), &overrides),
            Some("tab.new")
        );
        // 把别的命令绑到 ctrl+alt+n 会与 override 冲突
        let err = r.validate_override("tab.close", "ctrl+alt+n", &overrides).unwrap_err();
        assert!(err.contains("New Tab"), "{err}");
        assert!(r.effective_conflicts(&overrides).is_empty());
    }

    #[test]
    fn terminal_menu_commands_are_registered_without_default_shortcuts() {
        let r = builtin();
        for id in [
            "terminal.toggle-read-only",
            "terminal.reset",
            "terminal.reset-and-clear",
            "terminal.save-contents",
        ] {
            let cmd = r.get(id).unwrap_or_else(|| panic!("缺少命令 {id}"));
            assert_eq!(cmd.category, Category::Terminal);
            assert!(cmd.default_shortcut.is_none(), "{id} 不应占用默认快捷键");
        }
    }

    #[test]
    fn fuzzy_search_prefers_title_prefix() {
        let r = builtin();
        let hits = r.search("split");
        assert!(!hits.is_empty());
        assert!(hits[0].title.to_lowercase().contains("split"), "首个命中应是 Split: {}", hits[0].title);

        // 子序列也能命中
        let ids: Vec<&str> = r.search("pltt").iter().map(|c| c.id).collect();
        assert!(ids.contains(&"view.command-palette"), "{ids:?}");

        // 空查询返回全部
        assert_eq!(r.search("").len(), r.len());
        // 完全无关的查询返回空
        assert!(r.search("zzzzzz").is_empty());
    }
}
