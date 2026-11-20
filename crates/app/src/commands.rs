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
    /// 同一命令的附加默认绑定（例如 `Ctrl+=` 与 `Ctrl++` 都放大）。
    ///
    /// 这些绑定只用于分发/冲突检测/展示，不会出现在 `default_shortcut` 里；
    /// 用户 override 仍然只替换主绑定。
    pub extra_shortcuts: &'static [&'static str],
    /// 全局命令：分发早于搜索框与终端输入（F2/F11/Tab 导航等）。
    ///
    /// 非全局命令在搜索框打开时不触发，与旧硬编码路径的顺序保持一致。
    pub global: bool,
}

/// 构造命令的简写（无别名、非全局）。
fn bind(
    id: &'static str,
    title: &'static str,
    category: Category,
    default_shortcut: Option<&'static str>,
) -> Command {
    Command { id, title, category, default_shortcut, extra_shortcuts: &[], global: false }
}

/// 全局命令：搜索框打开/终端输入时也优先按命令表分发。
fn global_bind(
    id: &'static str,
    title: &'static str,
    category: Category,
    default_shortcut: Option<&'static str>,
) -> Command {
    Command { global: true, ..bind(id, title, category, default_shortcut) }
}

impl Command {
    /// 主绑定 + 附加绑定的解析结果（非法项被忽略）。
    fn default_shortcuts(&self) -> impl Iterator<Item = Shortcut> + '_ {
        self.default_shortcut
            .into_iter()
            .chain(self.extra_shortcuts.iter().copied())
            .filter_map(|text| Shortcut::parse(text).ok())
    }
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

    /// 有默认快捷键的命令（主绑定 + 附加绑定，解析成功才算）。
    pub fn bindings(&self) -> Vec<(Shortcut, &'static str)> {
        self.commands
            .iter()
            .flat_map(|c| c.default_shortcuts().map(move |sc| (sc, c.id)))
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

    /// 用户覆盖的完整快捷键（支持 1–2 段）；未知命令/非法配置返回 None。
    pub fn override_shortcut_for(
        &self,
        command_id: &str,
        overrides: &[HotkeyOverride],
    ) -> Option<Shortcut> {
        self.get(command_id)?;
        self.override_for(command_id, overrides)
            .and_then(|o| Shortcut::parse(&o.chord).ok())
    }

    /// 某条命令当前生效的绑定：合法 override 优先，否则回退到默认。
    pub fn effective_shortcut(
        &self,
        command_id: &str,
        overrides: &[HotkeyOverride],
    ) -> Option<Shortcut> {
        if let Some(shortcut) = self.override_shortcut_for(command_id, overrides) {
            return Some(shortcut);
        }
        self.get(command_id)?
            .default_shortcut
            .and_then(|s| Shortcut::parse(s).ok())
    }

    /// 全部命令当前生效的绑定（未知命令 / 非法 chord 的 override 被忽略）。
    ///
    /// 有 override 时只返回 override（主绑定被替换）；没有时返回主绑定与附加绑定。
    pub fn effective_bindings(&self, overrides: &[HotkeyOverride]) -> Vec<EffectiveBinding> {
        let mut out = Vec::new();
        for command in &self.commands {
            if let Some(custom) = self
                .override_for(command.id, overrides)
                .and_then(|o| Shortcut::parse(&o.chord).ok())
            {
                out.push(EffectiveBinding {
                    command_id: command.id,
                    shortcut: custom,
                    custom: true,
                });
                continue;
            }
            for shortcut in command.default_shortcuts() {
                out.push(EffectiveBinding { command_id: command.id, shortcut, custom: false });
            }
        }
        out
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

    /// 校验一条 1–2 段快捷键能否保存（Hotkeys 页录制用，支持 multi-chord）。
    ///
    /// 冲突规则：完全相同或前缀重叠都拒绝；命令自己的旧绑定会被替换，不算冲突。
    pub fn validate_override_shortcut(
        &self,
        command_id: &str,
        shortcut_text: &str,
        overrides: &[HotkeyOverride],
    ) -> Result<Shortcut, String> {
        self.get(command_id).ok_or_else(|| format!("未知命令: {command_id}"))?;
        let shortcut = Shortcut::parse(shortcut_text)?;
        if !shortcut.is_bindable() {
            return Err("快捷键需要 Ctrl/Alt/Meta，或使用 F1–F24".to_string());
        }
        for binding in self.effective_bindings(overrides) {
            if binding.command_id == command_id {
                continue;
            }
            if shortcuts_overlap(&binding.shortcut, &shortcut) {
                let title =
                    self.get(binding.command_id).map(|c| c.title).unwrap_or(binding.command_id);
                return Err(format!("冲突：{title} 已使用 {}", binding.shortcut));
            }
        }
        Ok(shortcut)
    }

    /// 某个单段 chord 是否已被命令的**默认**绑定占用（Profile 热键冲突提示用）。
    ///
    /// 只看 `default_shortcut`/`extra_shortcuts`，不读用户 override —— 冲突提示
    /// 是给用户看的，而 override 本身就是用户自己的选择。
    pub fn default_command_for_chord(&self, chord: &Chord) -> Option<&'static str> {
        self.all().iter().find_map(|command| {
            command
                .default_shortcuts()
                .find(|shortcut| shortcut.single_chord() == Some(chord))
                .map(|_| command.id)
        })
    }

    /// 全局命令的默认绑定（F2/F11/Tab 导航等），分发早于搜索框/终端输入。
    pub fn global_default_for_chord(&self, chord: &Chord) -> Option<&'static str> {
        self.all().iter().find_map(|command| {
            if !command.global {
                return None;
            }
            command
                .default_shortcuts()
                .find(|shortcut| shortcut.single_chord() == Some(chord))
                .map(|_| command.id)
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
            // 两段快捷键不参与单键分发，由 pending + command_for_sequence 处理。
            if Shortcut::parse(&over.chord)
                .is_ok_and(|bound| bound.single_chord() == Some(chord))
            {
                return Some(command.id);
            }
        }
        None
    }

    /// multi-chord 分发：第一段是否应该“等待第二段”。
    ///
    /// 只在“存在两段绑定以它为前缀、且它本身不是任何单段绑定”时返回 true，
    /// 避免把普通的单键命令延迟两秒。
    pub fn multi_chord_prefix(&self, first: &Chord, overrides: &[HotkeyOverride]) -> bool {
        if !first.is_bindable() {
            return false;
        }
        let bindings = self.effective_bindings(overrides);
        let is_prefix = bindings.iter().any(|binding| {
            binding.shortcut.chords.len() == 2 && binding.shortcut.chords[0] == *first
        });
        let is_complete = bindings
            .iter()
            .any(|binding| binding.shortcut.single_chord() == Some(first));
        is_prefix && !is_complete
    }

    /// multi-chord 分发：完整的 1–2 段序列命中哪条命令（override 优先于默认）。
    pub fn command_for_sequence(
        &self,
        chords: &[Chord],
        overrides: &[HotkeyOverride],
    ) -> Option<&'static str> {
        let wanted = Shortcut { chords: chords.to_vec() };
        let bindings = self.effective_bindings(overrides);
        if let Some(binding) =
            bindings.iter().rev().find(|binding| binding.custom && binding.shortcut == wanted)
        {
            return Some(binding.command_id);
        }
        self.default_command_for_sequence(&wanted)
    }

    /// 默认绑定里等于给定序列的命令（Hotkeys 页展示/分发共用）。
    pub fn default_command_for_sequence(&self, wanted: &Shortcut) -> Option<&'static str> {
        self.commands.iter().find_map(|command| {
            command
                .default_shortcuts()
                .any(|shortcut| shortcut == *wanted)
                .then_some(command.id)
        })
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
            let bound = Shortcut::parse(&self.override_for(command.id, overrides)?.chord).ok()?;
            (bound.single_chord() != Some(chord) && bound.is_bindable()).then_some(command.id)
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

/// 两条快捷键是否重叠：完全相同，或一条是另一条的前缀（multi-chord 的第一段）。
///
/// 前缀也算冲突：按了第一段后分发会等待第二段（见 `multi_chord_prefix`），
/// 若同一 chord 还是另一条命令的完整绑定，那条命令就永远等不到触发。
pub fn shortcuts_overlap(a: &Shortcut, b: &Shortcut) -> bool {
    let n = a.chords.len().min(b.chords.len());
    a.chords[..n] == b.chords[..n]
}

impl Shortcut {
    /// 所有段都必须可绑定（multi-chord 的每一段都带 Ctrl/Alt/Meta 或功能键）。
    pub fn is_bindable(&self) -> bool {
        !self.chords.is_empty() && self.chords.iter().all(Chord::is_bindable)
    }

    /// 解析 `ctrl+shift+p` 或 `ctrl+k ctrl+t`（空格分隔多段、`+` 连接修饰键）。
    pub fn parse(text: &str) -> Result<Self, String> {
        // 手写配置常见 `Ctrl + K Ctrl + T`：先去掉 `+` 两侧空白，再按空白分段，
        // 否则每个 "Ctrl" 都会被当成一段（缺少主键）。
        let compact = text.replace(" + ", "+").replace("+ ", "+").replace(" +", "+");
        let chords: Result<Vec<Chord>, String> =
            compact.split_whitespace().map(Chord::parse).collect();
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
            | "print"
            | "pause"
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

/// 写入/替换某命令的 1–2 段快捷键 override（规范格式，空格分隔两段）。
pub fn set_override_shortcut(
    overrides: &mut Vec<HotkeyOverride>,
    command: &str,
    shortcut: &Shortcut,
) {
    overrides.retain(|o| o.command != command);
    overrides.push(HotkeyOverride { command: command.to_string(), chord: shortcut.canonical() });
}

/// 清除某命令的 override；返回是否真的删掉了。
pub fn clear_override(overrides: &mut Vec<HotkeyOverride>, command: &str) -> bool {
    let before = overrides.len();
    overrides.retain(|o| o.command != command);
    overrides.len() != before
}

/// 一条被自动修复掉的快捷键配置（KEY-003）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverrideRepair {
    pub command: String,
    pub chord: String,
    /// `unknown-command` / `invalid-shortcut` / `not-bindable` /
    /// `duplicate-command` / `conflict`。
    pub reason: &'static str,
}

/// 启动/热重载时自动修复手改 `[[hotkeys]]` 的重复与非法项。
///
/// 规则与运行时分发语义一致：**后写的条目优先**（`override_for` 取最后一条）。
/// 从尾部向前扫描，保留第一条合法且不与已保留项冲突的配置；被丢弃的原因
/// 返回给调用方记录日志/写回配置。未知命令、非法 chord、不可绑定单键
/// （无修饰普通字符）、同一命令重复、与已保留项相同或前缀冲突都会被清掉。
pub fn repair_hotkey_overrides(
    registry: &Registry,
    overrides: &mut Vec<HotkeyOverride>,
) -> Vec<OverrideRepair> {
    let mut kept: Vec<HotkeyOverride> = Vec::new();
    let mut kept_shortcuts: Vec<Shortcut> = Vec::new();
    let mut repairs: Vec<OverrideRepair> = Vec::new();
    for over in overrides.iter().rev() {
        let shortcut = Shortcut::parse(&over.chord).ok();
        let reason = if registry.get(&over.command).is_none() {
            Some("unknown-command")
        } else if kept.iter().any(|k| k.command == over.command) {
            Some("duplicate-command")
        } else {
            match &shortcut {
                None => Some("invalid-shortcut"),
                Some(shortcut) if !shortcut.is_bindable() => Some("not-bindable"),
                Some(shortcut)
                    if kept_shortcuts
                        .iter()
                        .any(|kept| shortcuts_overlap(kept, shortcut)) =>
                {
                    Some("conflict")
                },
                Some(_) => None,
            }
        };
        match (reason, shortcut) {
            (None, Some(shortcut)) => {
                kept.push(over.clone());
                kept_shortcuts.push(shortcut);
            }
            (reason, _) => repairs.push(OverrideRepair {
                command: over.command.clone(),
                chord: over.chord.clone(),
                reason: reason.unwrap_or("invalid-shortcut"),
            }),
        }
    }
    kept.reverse();
    repairs.reverse();
    *overrides = kept;
    repairs
}

/// 桌面环境常见保留键（KEY-003 提示用；不阻止保存，只警告）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservedShortcut {
    /// 规范小写 chord（`meta+l`）；`ctrl+alt+f1..f12` 用特殊条目表示。
    pub chord: &'static str,
    /// 典型占用者（桌面环境/输入法）。
    pub owner: &'static str,
    /// 中文说明（UI 提示 + 日志）。
    pub note: &'static str,
}

/// Wayland 合成器/桌面环境/输入法的保留键清单。
///
/// 这些键由 DE 在客户端之前截获，绑到 Typer 里通常永远不会触发；录制时给出
/// 警告，但**不阻止保存**（用户可能用非 GNOME/KDE 环境或改过 DE 设置）。
pub const PLATFORM_RESERVED: &[ReservedShortcut] = &[
    ReservedShortcut { chord: "meta+l", owner: "GNOME/KDE", note: "锁屏" },
    ReservedShortcut { chord: "meta+d", owner: "GNOME", note: "显示桌面" },
    ReservedShortcut { chord: "meta+tab", owner: "GNOME", note: "切换窗口/活动" },
    ReservedShortcut { chord: "meta+space", owner: "IBus/fcitx", note: "切换输入法" },
    ReservedShortcut { chord: "meta+v", owner: "GNOME 扩展", note: "剪贴板历史" },
    ReservedShortcut { chord: "meta+1", owner: "GNOME", note: "Dock 第 1 个应用" },
    ReservedShortcut { chord: "meta+2", owner: "GNOME", note: "Dock 第 2 个应用" },
    ReservedShortcut { chord: "meta+3", owner: "GNOME", note: "Dock 第 3 个应用" },
    ReservedShortcut { chord: "meta+4", owner: "GNOME", note: "Dock 第 4 个应用" },
    ReservedShortcut { chord: "meta+5", owner: "GNOME", note: "Dock 第 5 个应用" },
    ReservedShortcut { chord: "meta+6", owner: "GNOME", note: "Dock 第 6 个应用" },
    ReservedShortcut { chord: "meta+7", owner: "GNOME", note: "Dock 第 7 个应用" },
    ReservedShortcut { chord: "meta+8", owner: "GNOME", note: "Dock 第 8 个应用" },
    ReservedShortcut { chord: "meta+9", owner: "GNOME", note: "Dock 第 9 个应用" },
    ReservedShortcut { chord: "alt+tab", owner: "桌面环境", note: "切换窗口" },
    ReservedShortcut { chord: "alt+f4", owner: "桌面环境", note: "关闭窗口" },
    ReservedShortcut { chord: "alt+space", owner: "桌面环境", note: "窗口菜单" },
    ReservedShortcut { chord: "ctrl+alt+t", owner: "GNOME/KDE", note: "打开终端" },
    ReservedShortcut { chord: "ctrl+alt+delete", owner: "桌面环境", note: "注销/锁屏" },
    ReservedShortcut { chord: "ctrl+alt+left", owner: "桌面环境", note: "切换工作区" },
    ReservedShortcut { chord: "ctrl+alt+right", owner: "桌面环境", note: "切换工作区" },
    ReservedShortcut { chord: "ctrl+alt+up", owner: "桌面环境", note: "切换工作区" },
    ReservedShortcut { chord: "ctrl+alt+down", owner: "桌面环境", note: "切换工作区" },
    ReservedShortcut { chord: "print", owner: "桌面环境", note: "截图" },
    ReservedShortcut {
        chord: "ctrl+alt+f1..f12",
        owner: "systemd/X11",
        note: "切换虚拟终端（切走后无法回到 Typer）",
    },
];

/// 某个 chord 是否被桌面环境保留；命中返回清单条目（含占用者与说明）。
pub fn platform_reserved_for(chord: &Chord) -> Option<&'static ReservedShortcut> {
    let canonical = chord.canonical();
    if let Some(hit) = PLATFORM_RESERVED.iter().find(|entry| entry.chord == canonical) {
        return Some(hit);
    }
    // Ctrl+Alt+F1–F12：切 VT，最危险的绑定。
    if chord.mods.ctrl && chord.mods.alt && !chord.mods.shift && !chord.mods.meta {
        if let Some(n) = chord.key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            if (1..=12).contains(&n) {
                return PLATFORM_RESERVED.last();
            }
        }
    }
    None
}

/// 一条快捷键（两段时看第一段）是否踩到平台保留键。
pub fn platform_reserved_for_shortcut(shortcut: &Shortcut) -> Option<&'static ReservedShortcut> {
    shortcut.chords.first().and_then(platform_reserved_for)
}

/// 内置命令集：一个用户可见操作对应一条。
///
/// 分发顺序由命令表决定：
/// - `global_bind`：搜索框打开也生效（F2 重命名、F11 全屏、Ctrl+Tab/Ctrl+1–9 导航）；
/// - 其余命令：搜索框关闭后、终端编码之前分发；
/// - `extra_shortcuts`：同一命令的等价默认键（如 `Ctrl+=` 与 `Ctrl++`）。
pub fn builtin() -> Registry {
    let mut r = Registry::new();
    let cmds = [
        bind("window.new", "New Window", Category::Window, Some("ctrl+shift+n")),
        bind("window.close", "Close Window", Category::Window, Some("ctrl+shift+q")),
        // GNOME Terminal：Ctrl+Shift+T 新建 Tab；Ctrl+T 留给 shell 的 transpose。
        bind("tab.new", "New Tab", Category::Workspace, Some("ctrl+shift+t")),
        bind("tab.search", "Search Tabs", Category::Workspace, Some("ctrl+shift+a")),
        bind("tab.close", "Close Tab", Category::Workspace, Some("ctrl+shift+w")),
        Command {
            extra_shortcuts: &["ctrl+tab"],
            ..global_bind("tab.next", "Next Tab", Category::Workspace, Some("ctrl+pagedown"))
        },
        Command {
            extra_shortcuts: &["ctrl+shift+tab"],
            ..global_bind("tab.prev", "Previous Tab", Category::Workspace, Some("ctrl+pageup"))
        },
        bind("tab.move-left", "Move Tab Left", Category::Workspace, Some("ctrl+shift+pageup")),
        bind("tab.move-right", "Move Tab Right", Category::Workspace, Some("ctrl+shift+pagedown")),
        bind("tab.last", "Last Used Tab", Category::Workspace, Some("ctrl+shift+l")),
        bind("tab.duplicate", "Duplicate Tab", Category::Workspace, None),
        bind("tab.reopen-closed", "Reopen Closed Tab", Category::Workspace, Some("ctrl+alt+shift+t")),
        global_bind("tab.rename", "Rename Tab", Category::Workspace, Some("f2")),
        bind("tab.rename-undo", "Undo Tab Rename", Category::Workspace, Some("ctrl+shift+z")),
        // Ctrl+1–9：跳转到第 n 个 Tab。全部是全局命令（搜索框开着也能切 Tab）。
        global_bind("tab.select-1", "Select Tab 1", Category::Workspace, Some("alt+1")),
        global_bind("tab.select-2", "Select Tab 2", Category::Workspace, Some("alt+2")),
        global_bind("tab.select-3", "Select Tab 3", Category::Workspace, Some("alt+3")),
        global_bind("tab.select-4", "Select Tab 4", Category::Workspace, Some("alt+4")),
        global_bind("tab.select-5", "Select Tab 5", Category::Workspace, Some("alt+5")),
        global_bind("tab.select-6", "Select Tab 6", Category::Workspace, Some("alt+6")),
        global_bind("tab.select-7", "Select Tab 7", Category::Workspace, Some("alt+7")),
        global_bind("tab.select-8", "Select Tab 8", Category::Workspace, Some("alt+8")),
        global_bind("tab.select-9", "Select Tab 9", Category::Workspace, Some("alt+9")),
        global_bind("tab.select-10", "Select Tab 10", Category::Workspace, Some("alt+0")),
        bind("pane.split-right", "Split Right", Category::Pane, Some("ctrl+shift+e")),
        bind("pane.split-down", "Split Down", Category::Pane, Some("ctrl+shift+d")),
        bind("pane.close", "Close Pane", Category::Pane, Some("ctrl+shift+x")),
        bind("pane.undo-swap", "Undo Pane Move", Category::Pane, Some("ctrl+alt+z")),
        // 方向聚焦：旧硬编码 Ctrl+Shift+方向键的等价命令。`pane.focus-next`
        // 作为“循环下一个”保留在命令表里，但不占用默认键（避免与右键冲突）。
        bind("pane.focus-next", "Focus Next Pane", Category::Pane, None),
        bind("pane.focus-left", "Focus Pane Left", Category::Pane, Some("ctrl+shift+left")),
        bind("pane.focus-right", "Focus Pane Right", Category::Pane, Some("ctrl+shift+right")),
        bind("pane.focus-up", "Focus Pane Up", Category::Pane, Some("ctrl+shift+up")),
        bind("pane.focus-down", "Focus Pane Down", Category::Pane, Some("ctrl+shift+down")),
        bind("terminal.copy", "Copy", Category::Terminal, Some("ctrl+shift+c")),
        bind("terminal.paste", "Paste", Category::Terminal, Some("ctrl+shift+v")),
        bind("terminal.find", "Find", Category::Terminal, Some("ctrl+shift+f")),
        bind("terminal.clear", "Clear Screen", Category::Terminal, None),
        bind("terminal.select-all", "Select All", Category::Terminal, None),
        bind("terminal.toggle-read-only", "Toggle Read Only", Category::Terminal, None),
        bind("terminal.toggle-broadcast", "Toggle Broadcast Input", Category::Terminal, None),
        bind("terminal.reset", "Reset Terminal", Category::Terminal, None),
        bind("terminal.reset-and-clear", "Reset and Clear", Category::Terminal, None),
        bind("terminal.save-contents", "Save Contents…", Category::Terminal, None),
        bind("pane.toggle-zoom", "Toggle Pane Zoom", Category::Pane, Some("ctrl+shift+m")),
        // UI-TAB-006：关闭范围操作的默认键（快捷键/面板/Hotkeys 共用）。
        bind("tab.close-others", "Close Other Tabs", Category::Workspace, Some("ctrl+alt+w")),
        bind("tab.close-left", "Close Tabs to the Left", Category::Workspace, Some("ctrl+alt+left")),
        bind("tab.close-right", "Close Tabs to the Right", Category::Workspace, Some("ctrl+alt+right")),
        // TERM-015：pane 级会话录制。录制状态/buffer 在 core，UI 只调用。
        bind("terminal.recording-start", "Start Recording", Category::Terminal, None),
        bind("terminal.recording-stop", "Stop Recording", Category::Terminal, None),
        bind("terminal.recording-export", "Export Recording…", Category::Terminal, None),
        bind("view.command-palette", "Command Palette", Category::View, Some("ctrl+shift+p")),
        global_bind("view.toggle-fullscreen", "Toggle Fullscreen", Category::View, Some("f11")),
        bind("view.sidebar", "Toggle Sidebar", Category::View, Some("ctrl+shift+b")),
        bind("view.toggle-quake", "Toggle Quake Mode", Category::View, None),
        bind("view.scroll-top", "Scroll to Top", Category::View, Some("ctrl+shift+home")),
        bind("view.scroll-bottom", "Scroll to Bottom", Category::View, Some("ctrl+shift+end")),
        bind("workspace.save", "Save Layout as Workspace", Category::View, None),
        bind("workspace.list", "List Workspaces", Category::View, None),
        bind("workspace.open", "Open Last Workspace", Category::View, None),
        // GNOME Terminal 同时支持 Ctrl++ 与 Ctrl+=（有些键盘/布局只送后者）。
        Command {
            // 旧硬编码在 Ctrl+Shift 下也接受“+”（shift+= 的真实文本）。
            extra_shortcuts: &["ctrl+=", "ctrl+shift++"],
            ..bind("view.zoom-in", "Zoom In", Category::View, Some("ctrl+plus"))
        },
        Command {
            extra_shortcuts: &["ctrl+-"],
            ..bind("view.zoom-out", "Zoom Out", Category::View, Some("ctrl+minus"))
        },
        bind("view.zoom-reset", "Reset Zoom", Category::View, Some("ctrl+0")),
        bind("session.restart", "Restart Session", Category::Session, None),
        bind("session.reattach", "Reattach Background Session", Category::Session, None),
        bind("settings.open", "Open Settings", Category::Settings, None),
        // 官方 SSH 插件 POC：ssh.connect 走 Backend::Plugin 打开 Profile 连接。
        // 官方 SSH 插件 POC：`ssh.connect` 开第一个 SSH Profile；调色板里
        // `ssh.connect:<id|name>` 精确指定 Profile（走 Backend::Plugin）。
        bind("ssh.connect", "Connect SSH (Plugin POC)", Category::Session, None),
    ];
    for command in cmds {
        r.register(command).expect("内置命令 ID 不能重复");
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
    fn tab_scope_close_commands_have_distinct_shortcuts() {
        let r = builtin();
        // UI-TAB-006：关闭其它/左/右不再只有右键菜单，命令面板与全局快捷键共用。
        for (id, shortcut, key) in [
            ("tab.close-others", "ctrl+alt+w", "w"),
            ("tab.close-left", "ctrl+alt+left", "left"),
            ("tab.close-right", "ctrl+alt+right", "right"),
        ] {
            let cmd = r.get(id).unwrap_or_else(|| panic!("{id} 必须注册"));
            assert_eq!(cmd.category, Category::Workspace);
            assert_eq!(cmd.default_shortcut, Some(shortcut));
            let chord = Chord::parse(shortcut).unwrap();
            assert_eq!(chord.key, key);
            assert_eq!(r.default_command_for_chord(&chord), Some(id));
            // 同一个 chord 只能落在一个命令上（兜底分发是 1:1 的）。
            let hits = r
                .all()
                .iter()
                .filter(|c| c.default_shortcut == Some(shortcut))
                .count();
            assert_eq!(hits, 1, "{shortcut} 不应被多个命令占用");
        }
        // Ctrl+W 必须留给 shell 的 unix-word-rubout（删除前一个词），
        // 否则终端里“删除”永远失灵；关闭 Tab 改用 Ctrl+Shift+W。
        assert!(
            r.default_command_for_chord(&Chord::parse("ctrl+w").unwrap()).is_none(),
            "Ctrl+W 不能绑定 Tervo 命令"
        );
        // 既有 Close Tab / Reopen 仍保留原绑定，改动不能踩到它们。
        assert_eq!(
            r.get("tab.close").and_then(|c| c.default_shortcut),
            Some("ctrl+shift+w")
        );
        assert_eq!(
            r.get("tab.reopen-closed").and_then(|c| c.default_shortcut),
            Some("ctrl+alt+shift+t")
        );
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
        r.register(bind("a.b", "A", Category::View, None)).unwrap();
        assert!(r.register(bind("a.b", "B", Category::View, None)).is_err());
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
        r.register(bind("x.one", "One", Category::View, Some("ctrl+1"))).unwrap();
        r.register(bind("x.two", "Two", Category::View, Some("ctrl+2"))).unwrap();
        r.register(bind("x.three", "Three", Category::View, None)).unwrap();
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
        r.register(bind("x.one", "One", Category::View, Some("ctrl+shift+x"))).unwrap();
        r.register(bind("x.two", "Two", Category::View, Some("ctrl+shift+x"))).unwrap();
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
            r.override_command_for_chord(&Chord::parse("ctrl+shift+t").unwrap(), &[]),
            None
        );
        assert!(r.suppressed_default(&Chord::parse("ctrl+shift+t").unwrap(), &[]).is_none());

        let mut overrides = Vec::new();
        let chord = r.validate_override("tab.new", "ctrl+alt+n", &overrides).unwrap();
        set_override(&mut overrides, "tab.new", &chord);

        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+alt+n").unwrap(), &overrides),
            Some("tab.new")
        );
        // 旧默认键被吞掉，避免 Ctrl+Shift+T 仍然新开 Tab
        assert_eq!(
            r.suppressed_default(&Chord::parse("ctrl+shift+t").unwrap(), &overrides),
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
    fn global_commands_replace_former_hardcoded_entries() {
        let r = builtin();
        // 旧硬编码 F2/F11/Tab 导航现在都是命令表里的 global 命令。
        for (chord, id) in [
            ("f2", "tab.rename"),
            ("f11", "view.toggle-fullscreen"),
            ("ctrl+pagedown", "tab.next"),
            ("ctrl+pageup", "tab.prev"),
            ("alt+1", "tab.select-1"),
            ("alt+5", "tab.select-5"),
            ("alt+9", "tab.select-9"),
            ("alt+0", "tab.select-10"),
        ] {
            let chord = Chord::parse(chord).unwrap();
            assert_eq!(r.global_default_for_chord(&chord), Some(id), "{chord}");
            assert_eq!(r.default_command_for_chord(&chord), Some(id), "{chord}");
        }
        // 非全局命令仍走“搜索框之后”的分发，不会被提到搜索框前面。
        for (chord, id) in [
            ("ctrl+shift+c", "terminal.copy"),
            ("ctrl+shift+t", "tab.new"),
            ("ctrl+shift+f", "terminal.find"),
        ] {
            let chord = Chord::parse(chord).unwrap();
            assert_eq!(r.global_default_for_chord(&chord), None, "{chord}");
            assert_eq!(r.default_command_for_chord(&chord), Some(id), "{chord}");
        }
    }

    #[test]
    fn command_table_covers_former_hardcoded_ctrl_blocks() {
        // 这两张表就是旧 `handle_input` 里 `if ctrl && shift` / `if ctrl`
        // 两段 match 的全集；迁到命令表后一条都不能丢。
        let r = builtin();
        for (chord, id) in [
            ("ctrl+shift+c", "terminal.copy"),
            ("ctrl+shift+v", "terminal.paste"),
            ("ctrl+shift+p", "view.command-palette"),
            ("ctrl+shift+a", "tab.search"),
            ("ctrl+shift+n", "window.new"),
            ("ctrl+shift+q", "window.close"),
            ("ctrl+shift+e", "pane.split-right"),
            ("ctrl+shift+d", "pane.split-down"),
            ("ctrl+shift+pageup", "tab.move-left"),
            ("ctrl+shift+pagedown", "tab.move-right"),
            ("ctrl+shift+w", "tab.close"),
            ("ctrl+shift+x", "pane.close"),
            ("ctrl+alt+shift+t", "tab.reopen-closed"),
            ("ctrl+shift+left", "pane.focus-left"),
            ("ctrl+shift+right", "pane.focus-right"),
            ("ctrl+shift+up", "pane.focus-up"),
            ("ctrl+shift+down", "pane.focus-down"),
            ("ctrl+shift++", "view.zoom-in"),
            ("ctrl+shift+t", "tab.new"),
            ("ctrl+shift+f", "terminal.find"),
            ("ctrl+=", "view.zoom-in"),
            ("ctrl++", "view.zoom-in"),
            ("ctrl+-", "view.zoom-out"),
            ("ctrl+0", "view.zoom-reset"),
        ] {
            let chord = Chord::parse(chord).unwrap();
            assert_eq!(
                r.default_command_for_chord(&chord),
                Some(id),
                "{chord} 应分发到 {id}"
            );
        }
        // 别名不产生冲突（同一命令的多个默认键）。
        assert!(r.conflicts().is_empty());
    }

    #[test]
    fn extra_shortcuts_are_listed_effective_and_dispatched() {
        let r = builtin();
        let zoom = r.get("view.zoom-in").unwrap();
        assert_eq!(zoom.default_shortcut, Some("ctrl+plus"));
        assert_eq!(zoom.extra_shortcuts, &["ctrl+=", "ctrl+shift++"]);
        let listed: Vec<String> = r
            .bindings()
            .into_iter()
            .filter(|(_, id)| *id == "view.zoom-in")
            .map(|(sc, _)| sc.canonical())
            .collect();
        assert_eq!(listed, vec!["ctrl++", "ctrl+=", "ctrl+shift++"]);

        // 生效绑定（Hotkeys 页/冲突检测）也包含附加键；主绑定仍优先展示。
        let effective: Vec<String> = r
            .effective_bindings(&[])
            .into_iter()
            .filter(|b| b.command_id == "view.zoom-in")
            .map(|b| b.shortcut.canonical())
            .collect();
        assert_eq!(effective.len(), 3);
        assert_eq!(
            r.effective_shortcut("view.zoom-in", &[]).unwrap().canonical(),
            "ctrl++"
        );
    }

    #[test]
    fn shortcut_overlap_covers_equality_and_prefix() {
        let single = Shortcut::parse("ctrl+k").unwrap();
        let multi = Shortcut::parse("ctrl+k ctrl+t").unwrap();
        let other = Shortcut::parse("ctrl+t").unwrap();
        assert!(shortcuts_overlap(&single, &multi), "前缀也是冲突");
        assert!(shortcuts_overlap(&multi, &single));
        assert!(shortcuts_overlap(&single, &single));
        assert!(!shortcuts_overlap(&single, &other));
        assert!(!shortcuts_overlap(&multi, &other));

        // 两段的每一段都必须可绑定。
        assert!(multi.is_bindable());
        assert!(!Shortcut::parse("ctrl+k a").unwrap().is_bindable());
        // 手写空格的单段/两段都能解析。
        assert_eq!(
            Shortcut::parse("Ctrl + K Ctrl + T").unwrap().canonical(),
            "ctrl+k ctrl+t"
        );
        assert_eq!(Shortcut::parse("Ctrl + Shift + N").unwrap().canonical(), "ctrl+shift+n");
    }

    #[test]
    fn multi_chord_validation_storage_and_dispatch() {
        let r = test_registry();
        let mut overrides = Vec::new();
        let multi = r.validate_override_shortcut("x.two", "ctrl+k ctrl+t", &overrides).unwrap();
        assert_eq!(multi.canonical(), "ctrl+k ctrl+t");
        set_override_shortcut(&mut overrides, "x.two", &multi);
        assert_eq!(
            r.override_shortcut_for("x.two", &overrides).unwrap().canonical(),
            "ctrl+k ctrl+t"
        );
        assert_eq!(
            r.effective_shortcut("x.two", &overrides).unwrap().canonical(),
            "ctrl+k ctrl+t"
        );

        // 第一段要等第二段；普通单段命令（x.one 的 ctrl+1）不该被延迟。
        let first = Chord::parse("ctrl+k").unwrap();
        assert!(r.multi_chord_prefix(&first, &overrides));
        assert!(!r.multi_chord_prefix(&Chord::parse("ctrl+2").unwrap(), &overrides));

        // 完整序列命中 override；只按第一段不算执行。
        let second = Chord::parse("ctrl+t").unwrap();
        assert_eq!(
            r.command_for_sequence(&[first.clone(), second.clone()], &overrides),
            Some("x.two")
        );
        assert_eq!(r.command_for_sequence(&[first.clone()], &overrides), None);
        // 单键 override 不再参与 multi 前缀。
        assert!(!r.multi_chord_prefix(&Chord::parse("ctrl+shift+y").unwrap(), &overrides));

        // 相同序列 → 冲突；与已有单段前缀重叠 → 也冲突。
        assert!(r.validate_override_shortcut("x.one", "ctrl+k ctrl+t", &overrides).is_err());
        let err = r.validate_override_shortcut("x.one", "ctrl+k", &overrides).unwrap_err();
        assert!(err.contains("冲突"), "{err}");
        // 改自己的键不算冲突（两段换一段）。
        assert!(r.validate_override_shortcut("x.two", "ctrl+shift+y", &overrides).is_ok());
    }

    #[test]
    fn multi_chord_override_does_not_hijack_single_chord_dispatch() {
        let r = test_registry();
        let mut overrides = Vec::new();
        set_override_shortcut(
            &mut overrides,
            "x.one",
            &Shortcut::parse("ctrl+k ctrl+t").unwrap(),
        );
        // override_command_for_chord 只认单段；两段由 command_for_sequence 处理。
        assert_eq!(
            r.override_command_for_chord(&Chord::parse("ctrl+k").unwrap(), &overrides),
            None
        );
        assert_eq!(
            r.command_for_sequence(
                &[Chord::parse("ctrl+k").unwrap(), Chord::parse("ctrl+t").unwrap()],
                &overrides
            ),
            Some("x.one")
        );
        // 旧默认键仍被吞掉（改键语义不变）。
        assert_eq!(
            r.suppressed_default(&Chord::parse("ctrl+1").unwrap(), &overrides),
            Some("x.one")
        );
    }

    #[test]
    fn repair_hotkey_overrides_drops_invalid_duplicate_and_conflicting_entries() {
        let r = builtin();
        let mut overrides = vec![
            // 后写的重复命令优先（与 override_for 语义一致）。
            HotkeyOverride { command: "tab.new".into(), chord: "ctrl+alt+n".into() },
            HotkeyOverride { command: "tab.new".into(), chord: "ctrl+alt+m".into() },
            // 未知命令 / 非法 / 无修饰普通字符。
            HotkeyOverride { command: "nope.command".into(), chord: "ctrl+alt+x".into() },
            HotkeyOverride { command: "tab.close".into(), chord: "ctrl+banana".into() },
            HotkeyOverride { command: "tab.search".into(), chord: "a".into() },
            // 同一命令重复：后写的 tab.rename ctrl+alt+shift+r 优先。
            HotkeyOverride { command: "tab.rename".into(), chord: "ctrl+alt+r".into() },
            HotkeyOverride { command: "tab.rename".into(), chord: "ctrl+alt+shift+r".into() },
            // 与保留项冲突（后写的 terminal.copy ctrl+alt+m 优先，tab.new 这条被丢）。
            HotkeyOverride { command: "terminal.copy".into(), chord: "ctrl+alt+m".into() },
        ];
        let repairs = repair_hotkey_overrides(&r, &mut overrides);
        // 保留下来的：tab.new(ctrl+alt+n)、tab.rename(ctrl+alt+shift+r)、terminal.copy(ctrl+alt+m)。
        assert_eq!(overrides.len(), 3, "修复结果: {overrides:?}");
        let kept: Vec<(&str, &str)> =
            overrides.iter().map(|o| (o.command.as_str(), o.chord.as_str())).collect();
        assert_eq!(
            kept,
            vec![
                ("tab.new", "ctrl+alt+n"),
                ("tab.rename", "ctrl+alt+shift+r"),
                ("terminal.copy", "ctrl+alt+m"),
            ]
        );
        let reasons: Vec<&str> = repairs.iter().map(|r| r.reason).collect();
        assert!(reasons.contains(&"duplicate-command"), "{reasons:?}");
        assert!(reasons.contains(&"unknown-command"), "{reasons:?}");
        assert!(reasons.contains(&"invalid-shortcut"), "{reasons:?}");
        assert!(reasons.contains(&"not-bindable"), "{reasons:?}");
        assert!(reasons.contains(&"conflict"), "{reasons:?}");
        // 修复结果必须无冲突、与分发一致。
        assert!(r.effective_conflicts(&overrides).is_empty());

        // 健康配置不被改动。
        let mut clean = vec![HotkeyOverride { command: "tab.new".into(), chord: "ctrl+alt+n".into() }];
        assert!(repair_hotkey_overrides(&r, &mut clean).is_empty());
        assert_eq!(clean.len(), 1);

        // 两段绑定：前缀重叠也算冲突。
        let mut prefix = vec![
            HotkeyOverride { command: "tab.new".into(), chord: "ctrl+k".into() },
            HotkeyOverride { command: "terminal.copy".into(), chord: "ctrl+k ctrl+t".into() },
        ];
        let repairs = repair_hotkey_overrides(&r, &mut prefix);
        assert_eq!(prefix.len(), 1, "{prefix:?}");
        assert_eq!(prefix[0].command, "terminal.copy", "后写优先");
        assert_eq!(repairs[0].reason, "conflict");
    }

    #[test]
    fn platform_reserved_table_covers_common_desktop_shortcuts() {
        let reserved = |text: &str| platform_reserved_for(&Chord::parse(text).unwrap()).is_some();
        assert!(reserved("meta+l"));
        assert!(reserved("alt+f4"));
        assert!(reserved("ctrl+alt+t"));
        assert!(reserved("ctrl+alt+f5"));
        assert!(reserved("meta+3"));
        // 普通应用快捷键不应被误报。
        assert!(!reserved("ctrl+t"));
        assert!(!reserved("ctrl+shift+c"));
        assert!(!reserved("f11"));
        // 两段快捷键看第一段。
        let multi = Shortcut::parse("alt+tab ctrl+t").unwrap();
        assert!(platform_reserved_for_shortcut(&multi).is_some());
        // 清单里的 chord 都能解析（防止拼写错误导致永远不提示）。
        for entry in PLATFORM_RESERVED {
            if entry.chord.contains("..") {
                continue;
            }
            assert!(
                Chord::parse(entry.chord).is_ok(),
                "保留键表里的 {} 无法解析",
                entry.chord
            );
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
