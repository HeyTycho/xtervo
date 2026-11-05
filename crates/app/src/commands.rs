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

    /// 检测重复绑定（同一个快捷键出现在多个命令上）。
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut seen: Vec<(Shortcut, &'static str)> = Vec::new();
        let mut out: Vec<Conflict> = Vec::new();
        for (sc, id) in self.bindings() {
            if let Some(conflict) = out.iter_mut().find(|c| c.shortcut == sc) {
                conflict.command_ids.push(id);
            } else if let Some((_, other)) = seen.iter().find(|(s, other)| *s == sc && *other != id) {
                out.push(Conflict { shortcut: sc.clone(), command_ids: vec![other, id] });
            } else {
                seen.push((sc, id));
            }
        }
        out
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
    fn parse(text: &str) -> Result<Self, String> {
        let mut mods = Mods::default();
        let mut key: Option<String> = None;
        for part in text.split('+') {
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
                    key = Some(normalize_key(other));
                },
            }
        }
        match key {
            Some(key) => Ok(Chord { mods, key }),
            None => Err(format!("缺少主键: {text}")),
        }
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

/// 键名归一化：`esc`→`escape`、单字符转小写，其余保持。
fn normalize_key(key: &str) -> String {
    match key {
        "esc" => "escape".to_string(),
        "return" => "enter".to_string(),
        "del" => "delete".to_string(),
        "pgup" => "pageup".to_string(),
        "pgdn" => "pagedown".to_string(),
        other => other.to_lowercase(),
    }
}

/// 内置命令集：一个用户可见操作对应一条。
pub fn builtin() -> Registry {
    let mut r = Registry::new();
    let cmds = [
        ("window.new", "New Window", Category::Window, Some("ctrl+shift+n")),
        ("tab.new", "New Tab", Category::Workspace, Some("ctrl+t")),
        ("tab.close", "Close Tab", Category::Workspace, Some("ctrl+w")),
        ("tab.next", "Next Tab", Category::Workspace, Some("ctrl+tab")),
        ("tab.prev", "Previous Tab", Category::Workspace, Some("ctrl+shift+tab")),
        ("tab.duplicate", "Duplicate Tab", Category::Workspace, None),
        ("tab.reopen-closed", "Reopen Closed Tab", Category::Workspace, Some("ctrl+shift+t")),
        ("pane.split-right", "Split Right", Category::Pane, Some("ctrl+shift+e")),
        ("pane.split-down", "Split Down", Category::Pane, Some("ctrl+shift+d")),
        ("pane.close", "Close Pane", Category::Pane, Some("ctrl+shift+w")),
        ("pane.focus-next", "Focus Next Pane", Category::Pane, Some("ctrl+shift+right")),
        ("terminal.copy", "Copy", Category::Terminal, Some("ctrl+shift+c")),
        ("terminal.paste", "Paste", Category::Terminal, Some("ctrl+shift+v")),
        ("terminal.find", "Find", Category::Terminal, Some("ctrl+f")),
        ("terminal.clear", "Clear Screen", Category::Terminal, None),
        ("terminal.select-all", "Select All", Category::Terminal, None),
        ("view.command-palette", "Command Palette", Category::View, Some("ctrl+shift+p")),
        ("view.toggle-fullscreen", "Toggle Fullscreen", Category::View, Some("f11")),
        ("view.toggle-quake", "Toggle Quake Mode", Category::View, None),
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

        let multi = Shortcut::parse("ctrl+k ctrl+t").unwrap();
        assert_eq!(multi.chords.len(), 2);

        assert!(Shortcut::parse("ctrl+").is_err());
        assert!(Shortcut::parse("ctrl+a+b").is_err());
        assert!(Shortcut::parse("ctrl+a ctrl+b ctrl+c").is_err());
        // 别名归一化
        assert_eq!(Shortcut::parse("esc").unwrap().chords[0].key, "escape");
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
