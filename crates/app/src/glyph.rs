// SPDX-License-Identifier: GPL-3.0-only
//! 等宽字形图集：fontdue 光栅化 + 单通道 atlas 打包。
//!
//! 只在下一次出现新字符时栅格化，之后每帧只做 coverage 混合。
//!
//! 字体链（按顺序、全部惰性加载）：
//! 1. 主等宽字体（JetBrains Mono / DejaVu Sans Mono）；
//! 2. CJK 回退（WQY Micro Hei / Droid Sans Fallback / Noto Sans CJK / WQY Zen Hei）；
//! 3. 符号与单色 emoji（Noto Sans Symbols 2 / DejaVu Sans / Symbola / Noto Emoji）；
//! 4. 用户用 `TERVO_FALLBACK_FONTS` 追加的字体（常见用途：Nerd Font 图标）；
//! 5. 彩色位图 emoji（Noto Color Emoji）只用来判断“系统确实有该字符”，
//!    fontdue 没有 CBDT/CBLC/COLR 位图光栅化能力，取不到轮廓时继续往后找，
//!    最后由渲染层画明确占位框，绝不画 tofu、也不 panic。

use std::collections::HashMap;
use std::sync::OnceLock;

use fontdue::{Font, FontSettings};

pub const ATLAS_W: usize = 1024;
pub const ATLAS_H: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Style {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl Style {
    #[inline]
    fn idx(self) -> usize {
        match self {
            Style::Regular => 0,
            Style::Bold => 1,
            Style::Italic => 2,
            Style::BoldItalic => 3,
        }
    }

    #[inline]
    fn is_bold(self) -> bool {
        matches!(self, Style::Bold | Style::BoldItalic)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Glyph {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    /// 位图左边相对笔位的偏移
    pub left: i16,
    /// 位图顶边相对基线的偏移（向下为负）
    pub top: i16,
    /// 该字形的步进宽度（像素），宽字符居中时用
    pub advance: u16,
}

/// 主字体候选：四个字重/斜体。
struct Candidate {
    regular: &'static str,
    bold: &'static str,
    italic: &'static str,
    bold_italic: &'static str,
}

const FONT_SETS: &[Candidate] = &[
    Candidate {
        regular: "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Medium.ttf",
        bold: "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Bold.ttf",
        italic: "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Italic.ttf",
        bold_italic: "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-BoldItalic.ttf",
    },
    Candidate {
        regular: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        bold: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
        italic: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Oblique.ttf",
        bold_italic: "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-BoldOblique.ttf",
    },
];

/// 回退候选按用途分组：缺中文时只碰中文组，缺 emoji 时不会为了找字形去解析
/// 18 MB 的 Noto CJK（否则第一个 emoji 就能把 RSS 推高几百 MB）。
const GROUP_CJK: usize = 0;
const GROUP_SYMBOL: usize = 1;
const GROUP_EMOJI: usize = 2;
const GROUP_ICON: usize = 3;
const GROUP_COLOR: usize = 4;
const GROUP_COUNT: usize = 5;

/// 回退字体候选。TTC 需要指定 face index：
/// Noto Sans (Mono) CJK SC 在 Regular/Bold TTC 里都是第 7 个 face。
///
/// `bold_path` 为 `None` 表示该字体没有单独粗体文件，粗体复用常规体（不画 tofu）。
struct FallbackCandidate {
    regular: &'static str,
    regular_index: u32,
    bold_path: Option<&'static str>,
    bold_index: u32,
    group: usize,
    /// 只有彩色位图（CBDT/CBLC），fontdue 无法取轮廓；命中后直接占位，不再扫后续字体。
    color_only: bool,
}

const FALLBACK_SETS: &[FallbackCandidate] = &[
    // --- CJK ---
    // 4.9 MB / ~106 ms：日常首选，启动开销可接受
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_CJK,
        color_only: false,
    },
    // 3.8 MB / ~70 ms：无粗体、无拉丁，但覆盖全，作为轻量兜底
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_CJK,
        color_only: false,
    },
    // 18 MB / ~260-335 ms：最好看，但只在没有轻量字体时才用
    FallbackCandidate {
        regular: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        regular_index: 7, // Noto Sans Mono CJK SC
        bold_path: Some("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"),
        bold_index: 7,
        group: GROUP_CJK,
        color_only: false,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        regular_index: 1, // WenQuanYi Zen Hei Mono
        bold_path: None,
        bold_index: 1,
        group: GROUP_CJK,
        color_only: false,
    },
    // --- 符号 / 单色 emoji ---
    // 先于 Noto Color Emoji：fontdue 只能栅格化轮廓，单色 emoji 能真的画出来。
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_SYMBOL,
        color_only: false,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_SYMBOL,
        color_only: false,
    },
    // Ubuntu fonts-symbola：单色 emoji 覆盖极好（含 ZWJ 家族），有则优先用。
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/ancient-scripts/Symbola_hint.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_EMOJI,
        color_only: false,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/noto/NotoEmoji-Regular.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_EMOJI,
        color_only: false,
    },
    // --- Nerd Font 常见系统安装路径（用户目录用 TERVO_FALLBACK_FONTS 追加） ---
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/nerd-fonts/SymbolsNerdFontMono-Regular.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_ICON,
        color_only: false,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/TTF/SymbolsNerdFontMono-Regular.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_ICON,
        color_only: false,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/opentype/nerd-fonts/SymbolsNerdFontMono-Regular.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_ICON,
        color_only: false,
    },
    // --- 彩色位图 emoji（最后）---
    // fontdue 不支持 CBDT/CBLC；保留它是为了知道系统有该 emoji 并回退到占位框。
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_COLOR,
        color_only: true,
    },
];

fn load_font(path: &str) -> Option<Font> {
    load_font_at(path, 0)
}

fn load_font_at(path: &str, collection_index: u32) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    let settings = FontSettings { collection_index, ..Default::default() };
    Font::from_bytes(bytes, settings).ok()
}

/// `TERVO_FONTLOG=1` 时打印字体解析耗时/失败原因，方便现场排查缺字。
fn fontlog() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TERVO_FONTLOG").is_some())
}

/// 一个回退候选，两个槽位各自惰性解析；失败结果也缓存，避免逐字重试。
struct FallbackFace {
    path: String,
    index: u32,
    bold: Option<(String, u32)>,
    group: usize,
    color_only: bool,
    is_env: bool,
    regular: OnceLock<Option<Font>>,
    bold_face: OnceLock<Option<Font>>,
}

impl FallbackFace {
    fn new(
        path: &str,
        index: u32,
        bold: Option<(&str, u32)>,
        group: usize,
        color_only: bool,
        env: bool,
    ) -> Self {
        Self {
            path: path.to_string(),
            index,
            bold: bold.map(|(p, i)| (p.to_string(), i)),
            group,
            color_only,
            is_env: env,
            regular: OnceLock::new(),
            bold_face: OnceLock::new(),
        }
    }

    fn load_font<'a>(
        &'a self,
        path: &str,
        index: u32,
        slot: &'a OnceLock<Option<Font>>,
    ) -> Option<&'a Font> {
        slot.get_or_init(|| {
            let started = std::time::Instant::now();
            let font = load_font_at(path, index);
            if fontlog() {
                match &font {
                    Some(_) => eprintln!("[font] 回退加载 {path}#{index} ({:?})", started.elapsed()),
                    None => eprintln!(
                        "[font] 回退不可用 {path}#{index}（{} 后放弃）",
                        started.elapsed().as_millis()
                    ),
                }
            }
            font
        })
        .as_ref()
    }

    /// 取常规/粗体字形；没有粗体文件时用常规体顶替。
    fn load(&self, bold: bool) -> Option<&Font> {
        if bold {
            if let Some((path, index)) = &self.bold {
                return self.load_font(path, *index, &self.bold_face);
            }
        }
        self.load_font(&self.path, self.index, &self.regular)
    }
}

struct Fallback {
    faces: Vec<FallbackFace>,
    /// 每个用途组的候选下标；`glyph_face` 按字符类型只走相关组。
    groups: [Vec<usize>; GROUP_COUNT],
}

/// 不同字符类型对应的回退组顺序。
///
/// - 中文：CJK → 符号 → emoji → 彩色 emoji（不碰图标组，Nerd 字体不该抢中文）
/// - emoji：符号（DejaVu 单色）→ 单色 emoji → 彩色 emoji；**不扫 CJK**
/// - PUA/Nerd 图标：图标 → 符号；没有 Nerd 字体时直接占位，不去解析 18 MB CJK
/// - 其它：符号 → CJK → emoji → 彩色
fn group_order(ch: char) -> &'static [usize] {
    use PlaceholderKind::*;
    match placeholder_kind(ch) {
        Some(Emoji) => &[GROUP_SYMBOL, GROUP_EMOJI, GROUP_COLOR],
        Some(Icon) => &[GROUP_ICON, GROUP_SYMBOL],
        None if is_cjk(ch) => &[GROUP_CJK, GROUP_SYMBOL, GROUP_EMOJI, GROUP_COLOR, GROUP_ICON],
        None => &[GROUP_SYMBOL, GROUP_CJK, GROUP_EMOJI, GROUP_COLOR, GROUP_ICON],
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF |      // Hangul Jamo
        0x2E80..=0xA4CF |      // CJK 部首、假名、汉字、彝文、注音
        0xAC00..=0xD7AF |      // Hangul 音节
        0xF900..=0xFAFF |      // CJK 兼容汉字
        0xFE30..=0xFE4F |      // CJK 兼容形式
        0xFF00..=0xFFEF |      // 全角字符
        0x20000..=0x3FFFF      // 扩展 B 及以后
    )
}

pub struct FontSet {
    faces: [Font; 4],
    /// 回退链：首次遇到主字体缺字时才建立（只建立空壳，字体本身仍逐个按需解析）。
    fallback: OnceLock<Fallback>,
    pub size: f32,
    pub cell_w: usize,
    pub cell_h: usize,
    /// 行顶部到基线的像素距离
    pub baseline: i32,
    #[allow(dead_code)]
    pub font_path: String,
}

impl FontSet {
    pub fn load(size: f32) -> Option<Self> {
        for cand in FONT_SETS {
            let Some(regular) = load_font(cand.regular) else { continue };
            let bold = load_font(cand.bold).unwrap_or_else(|| load_font(cand.regular).unwrap());
            let italic = load_font(cand.italic).unwrap_or_else(|| load_font(cand.regular).unwrap());
            let bold_italic =
                load_font(cand.bold_italic).unwrap_or_else(|| load_font(cand.regular).unwrap());

            let adv = regular.metrics('M', size).advance_width;
            let line = regular.horizontal_line_metrics(size)?;
            let cell_w = adv.round().max(1.0) as usize;
            // +2：给上伸部/下伸部留 1px 余量，避免首行字形被裁
            let cell_h = line.new_line_size.round().max(1.0) as usize + 2;
            let baseline = line.ascent.round() as i32 + 1;

            // 回退字体按需加载：这里连候选列表都还没建立，只显示 ASCII 的会话
            // 不解析任何 CJK/符号字体（wqy-microhei 解析后约占 170 MB）。
            return Some(Self {
                faces: [regular, bold, italic, bold_italic],
                fallback: OnceLock::new(),
                size,
                cell_w,
                cell_h,
                baseline,
                font_path: cand.regular.to_string(),
            });
        }
        None
    }

    #[allow(dead_code)]
    #[inline]
    pub fn face(&self, style: Style) -> &Font {
        &self.faces[style.idx()]
    }

    /// 回退链（仅第一次调用时建立；不解析字体）。
    #[inline]
    fn fallback(&self) -> &Fallback {
        self.fallback.get_or_init(load_fallback)
    }

    /// 第 `skip` 个**声称覆盖** `ch` 的字体：0 = 主字体，其后按回退链顺序。
    ///
    /// 返回值第二项 `true` 表示这是只有彩色位图（CBDT/CBLC）的字体：`has_glyph`
    /// 为真但 fontdue 取不到轮廓。栅格化层遇到它就直接放弃后续查找并交给占位框，
    /// 避免为一个 emoji 去解析整条 CJK 字体链。单色 emoji 字体在组顺序上先于它。
    pub fn glyph_face(
        &self,
        ch: char,
        style: Style,
        mut skip: usize,
    ) -> Option<(&Font, bool)> {
        let primary = &self.faces[style.idx()];
        if primary.has_glyph(ch) {
            if skip == 0 {
                return Some((primary, false));
            }
            skip -= 1;
        }
        let bold = style.is_bold();
        let fallback = self.fallback();
        for group in group_order(ch) {
            for &i in &fallback.groups[*group] {
                let face = &fallback.faces[i];
                let Some(font) = face.load(bold) else { continue };
                if font.has_glyph(ch) {
                    if skip == 0 {
                        return Some((font, face.color_only));
                    }
                    skip -= 1;
                }
            }
        }
        None
    }

    /// 字体链里是否有字体声称覆盖该字符（含彩色 emoji；用于测试/诊断）。
    #[allow(dead_code)]
    pub fn has_glyph(&self, ch: char) -> bool {
        // ASCII/主字体覆盖是最常见路径：短路后完全不触碰回退链。
        if self.faces.iter().any(|f| f.has_glyph(ch)) {
            return true;
        }
        self.fallback().faces.iter().any(|face| {
            face.load(false).is_some_and(|f| f.has_glyph(ch))
                || (face.bold.is_some() && face.load(true).is_some_and(|f| f.has_glyph(ch)))
        })
    }

    /// 字体链建立后的候选数量（诊断/测试用）。
    #[allow(dead_code)]
    pub fn fallback_len(&self) -> usize {
        self.fallback().faces.len()
    }
}

/// 建立回退链：内置候选 + `TERVO_FALLBACK_FONTS` 追加项。
///
/// 环境变量格式：`:` 分隔的字体路径，可带 `#face_index`（TTC），例如
/// `TERVO_FALLBACK_FONTS=/path/SymbolsNerdFontMono-Regular.ttf`。
fn load_fallback() -> Fallback {
    struct Spec {
        path: String,
        index: u32,
        bold: Option<(String, u32)>,
        group: usize,
        color_only: bool,
        env: bool,
    }
    let mut specs: Vec<Spec> = FALLBACK_SETS
        .iter()
        .map(|cand| Spec {
            path: cand.regular.to_string(),
            index: cand.regular_index,
            bold: cand.bold_path.map(|p| (p.to_string(), cand.bold_index)),
            group: cand.group,
            color_only: cand.color_only,
            env: false,
        })
        .collect();
    if let Ok(raw) = std::env::var("TERVO_FALLBACK_FONTS") {
        for entry in raw.split(':') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (path, index) = match entry.rsplit_once('#') {
                Some((p, idx)) => match idx.parse() {
                    Ok(i) => (p, i),
                    Err(_) => (entry, 0),
                },
                None => (entry, 0),
            };
            specs.push(Spec {
                path: path.to_string(),
                index,
                bold: None,
                group: GROUP_SYMBOL,
                color_only: false,
                env: true,
            });
        }
    }
    // 去重：同一路径/face 只解析一次；内置候选里可能和用户追加项重复。
    let mut seen = std::collections::HashSet::new();
    let mut faces = Vec::with_capacity(specs.len());
    let mut env_indices = Vec::new();
    for spec in specs {
        if !seen.insert((spec.path.clone(), spec.index)) {
            continue;
        }
        let bold = match spec.bold {
            Some((p, i)) if !(p == spec.path && i == spec.index) => Some((p, i)),
            _ => None,
        };
        if spec.env {
            env_indices.push(faces.len());
        }
        faces.push(FallbackFace::new(
            &spec.path,
            spec.index,
            bold.as_ref().map(|(p, i)| (p.as_str(), *i)),
            spec.group,
            spec.color_only,
            spec.env,
        ));
    }
    let mut groups: [Vec<usize>; GROUP_COUNT] = Default::default();
    // 用户显式配置的字体优先于内置符号字体（常见：Nerd Font）。放在 SYMBOL 组，
    // emoji/图标/普通缺字的回退顺序都会先经过它；CJK 字符仍优先用内置 CJK 字体。
    groups[GROUP_SYMBOL] = env_indices;
    for (i, face) in faces.iter().enumerate() {
        if !face.is_env {
            groups[face.group].push(i);
        }
    }
    if fontlog() {
        eprintln!("[font] 回退链 {} 个候选（按需解析）", faces.len());
    }
    Fallback { faces, groups }
}

/// 完全缺失时的明确占位类型。渲染层画空框，而不是字体的 .notdef tofu。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceholderKind {
    Emoji,
    Icon,
}

/// 零宽连接符/变体选择符：不画字形，但不能让它们单独触发占位框。
#[inline]
pub fn is_zero_width_control(ch: char) -> bool {
    matches!(ch,
        '\u{200B}'..='\u{200F}' |
        '\u{2060}'..='\u{206F}' |
        '\u{FEFF}' |
        '\u{FE00}'..='\u{FE0F}' |
        '\u{E0100}'..='\u{E01EF}'
    )
}

fn is_private_use(cp: u32) -> bool {
    matches!(cp, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

fn is_emoji(cp: u32) -> bool {
    matches!(
        cp,
        // Emoji / 补充符号 / 旗帜 / 肤色修饰符
        0x1F000..=0x1FAFF |
        // Dingbats（✔ ✨ ❤ 等）
        0x2600..=0x27BF |
        // 杂项符号与箭头（⌚ ⏰ ⏳ 等）
        0x2300..=0x23FF |
        // 几何图形（▪ ▶ ◼ 等）
        0x25A0..=0x25FF |
        // 杂项符号与箭头（⭐ ⬛ 等）
        0x2B00..=0x2BFF
    )
}

/// 字体链完全画不出来时，是否为 emoji / Nerd Font 私用区图标（值得画占位框）。
///
/// 普通缺字（生僻 CJK 等）返回 `None`，保持原来的留空行为，避免满屏空框。
pub fn placeholder_kind(ch: char) -> Option<PlaceholderKind> {
    if is_zero_width_control(ch) {
        return None;
    }
    let cp = ch as u32;
    if is_private_use(cp) {
        return Some(PlaceholderKind::Icon);
    }
    if is_emoji(cp) {
        return Some(PlaceholderKind::Emoji);
    }
    None
}

pub struct GlyphCache {
    atlas: Vec<u8>,
    pen_x: usize,
    pen_y: usize,
    row_h: usize,
    /// (字符, 样式, 占格数)：同一字符可能在 1 格/2 格上下文里缩放不同。
    map: HashMap<(char, Style, u8), Option<Glyph>>,
    pub rasterized: u64,
    pub atlas_full: u64,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self {
            atlas: vec![0u8; ATLAS_W * ATLAS_H],
            pen_x: 0,
            pen_y: 0,
            row_h: 0,
            map: HashMap::with_capacity(512),
            rasterized: 0,
            atlas_full: 0,
        }
    }
}

impl GlyphCache {
    pub fn atlas(&self) -> &[u8] {
        &self.atlas
    }

    #[allow(dead_code)]
    pub fn cached_count(&self) -> usize {
        self.map.len()
    }

    /// 取字形；`span` 是该格在网格里占的格数（宽字符/emoji = 2）。
    ///
    /// 字形步进超过 `span * cell_w` 时按比例缩小字号重新栅格化，保证不会撑破网格
    /// （典型：U+2764 这类默认窄、VS16 后想以 emoji 显示的字符只有 1 格）。
    pub fn get(&mut self, fonts: &FontSet, ch: char, style: Style, span: usize) -> Option<Glyph> {
        let span = span.clamp(1, u8::MAX as usize) as u8;
        if let Some(g) = self.map.get(&(ch, style, span)) {
            return *g;
        }
        let glyph = self.rasterize(fonts, ch, style, span as usize);
        self.map.insert((ch, style, span), glyph);
        glyph
    }

    fn rasterize(&mut self, fonts: &FontSet, ch: char, style: Style, span: usize) -> Option<Glyph> {
        let limit = (span.max(1) * fonts.cell_w) as f32;
        let mut skip = 0usize;
        // 依次尝试主字体 + 回退链里所有声称覆盖该字符的字体；彩色位图 emoji 命中
        // 会直接返回 None（由渲染层画占位框），单色 emoji 字体在组顺序上先于它。
        while let Some((font, color_only)) = fonts.glyph_face(ch, style, skip) {
            skip += 1;
            // 彩色位图 emoji：cmap 里有这个码位，但 fontdue 取不到轮廓。直接交给
            // 占位框，不再为它解析后面的 CJK 字体（一个 emoji 不该吃几百 MB RSS）。
            if color_only {
                return None;
            }
            let Some((metrics, bitmap)) = rasterize_fit(font, ch, fonts.size, limit) else {
                continue;
            };
            let (w, h) = (metrics.width, metrics.height);
            if w + 2 > ATLAS_W || h + 2 > ATLAS_H {
                continue;
            }
            if self.pen_x + w + 1 > ATLAS_W {
                self.pen_x = 0;
                self.pen_y += self.row_h + 1;
                self.row_h = 0;
            }
            if self.pen_y + h + 1 > ATLAS_H {
                self.atlas_full += 1;
                return None;
            }

            // 写入 atlas
            for row in 0..h {
                let dst = (self.pen_y + row) * ATLAS_W + self.pen_x;
                let src = row * w;
                self.atlas[dst..dst + w].copy_from_slice(&bitmap[src..src + w]);
            }

            let glyph = Glyph {
                x: self.pen_x as u16,
                y: self.pen_y as u16,
                w: w as u16,
                h: h as u16,
                left: metrics.xmin as i16,
                top: -(metrics.ymin + h as i32) as i16,
                advance: metrics.advance_width.round().max(0.0) as u16,
            };

            self.pen_x += w + 1;
            self.row_h = self.row_h.max(h);
            self.rasterized += 1;
            return Some(glyph);
        }
        None
    }
}

/// 在 `base_px` 下栅格化；步进/位图宽超过 `limit` 时等比缩小字号，保证塞进 `span`。
fn rasterize_fit(
    font: &Font,
    ch: char,
    base_px: f32,
    limit: f32,
) -> Option<(fontdue::Metrics, Vec<u8>)> {
    let mut px = base_px;
    let mut metrics = font.metrics(ch, px);
    if metrics.width == 0 || metrics.height == 0 {
        // 彩色位图 emoji：cmap 有该码位，但没有可轮廓化的字形。
        return None;
    }
    let widest = metrics.advance_width.max(metrics.width as f32);
    if widest > limit {
        let scaled = (px * limit / widest).max(1.0);
        if scaled < px - 0.01 {
            px = scaled;
            metrics = font.metrics(ch, px);
        }
    }
    if metrics.width == 0 || metrics.height == 0 {
        return None;
    }
    let (metrics, bitmap) = font.rasterize(ch, px);
    if metrics.width == 0 || metrics.height == 0 {
        return None;
    }
    Some((metrics, bitmap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Option<FontSet> {
        let f = FontSet::load(15.0);
        if f.is_none() {
            eprintln!("跳过：没有可用字体");
        }
        f
    }

    #[test]
    fn cjk_fallback_rasterizes() {
        let Some(fonts) = fonts() else { return };
        // 主字体 JetBrains Mono 没有中文字形，必须由回退字体接住。
        assert!(fonts.face(Style::Regular).has_glyph('A'));
        assert!(!fonts.face(Style::Regular).has_glyph('你'), "预期主字体缺中文");
        assert!(fonts.has_glyph('你'));
        assert!(fonts.has_glyph('好'));

        let mut cache = GlyphCache::default();
        let g = cache.get(&fonts, '你', Style::Regular, 2).expect("中文应能栅格化");
        assert!(g.w >= 8 && g.h >= 8, "中文位图过小: {g:?}");
        assert!(g.advance >= 10, "中文步进异常: {g:?}");
        // 回退字体没有粗体时也必须能画（用常规体顶替，不能掉回主字体变 tofu）
        assert!(cache.get(&fonts, '你', Style::Bold, 2).is_some(), "粗体中文丢字形");
        assert!(cache.get(&fonts, '好', Style::Italic, 2).is_some(), "斜体中文丢字形");
    }

    #[test]
    fn latin_uses_primary_face() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::default();
        let g = cache.get(&fonts, 'M', Style::Regular, 1).unwrap();
        // cell 宽就是按 'M' 的步进算的；缩放入口不能把主字体 ASCII 改小。
        assert_eq!(g.advance as usize, fonts.cell_w);
    }

    #[test]
    fn fallback_is_lazy() {
        let Some(fonts) = fonts() else { return };
        // 加载主字体时不该建立/解析回退链。
        assert!(fonts.fallback.get().is_none(), "FontSet::load 不应解析回退字体");

        // ASCII 的查询和栅格化只走主字体，同样不能触发回退加载。
        assert!(fonts.has_glyph('A'));
        let mut cache = GlyphCache::default();
        assert!(cache.get(&fonts, 'A', Style::Regular, 1).is_some());
        assert!(fonts.fallback.get().is_none(), "ASCII 渲染不应触发回退加载");

        // 主字体缺中文时才真正建立回退链（若系统没有 CJK 字体则跳过）。
        if fonts.has_glyph('你') {
            assert!(fonts.fallback.get().is_some(), "缺字时应按需建立回退链");
        }
    }

    #[test]
    fn emoji_uses_monochrome_outline_when_available() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::default();
        match cache.get(&fonts, '😀', Style::Regular, 2) {
            Some(g) => {
                assert!(g.w > 0 && g.h > 0, "emoji 位图为空: {g:?}");
                // 😀 在 unicode-width 里是 2 格；回退的 DejaVu 轮廓可用时不能只画半格。
                assert!(g.advance as usize >= fonts.cell_w, "emoji 步进过窄: {g:?}");
            },
            None => eprintln!("跳过：系统没有可直接轮廓化的 emoji 回退字体"),
        }
        // 彩色位图 emoji 字体不能崩溃：取不到轮廓时返回 None（渲染层画占位框）；
        // 若系统装了 Symbola / Noto Emoji 等单色字体，这里也可能是 Some，不能硬断言。
        let _ = cache.get(&fonts, '👨', Style::Regular, 2);
        let _ = cache.get(&fonts, '🚀', Style::Regular, 2);
    }

    #[test]
    fn narrow_emoji_is_scaled_into_one_cell() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::default();
        // ❤ 是窄字符（VS16 不改变网格宽度）：DejaVu 的原生 emoji 步进约 12px，
        // 比 15px 字号下的单格更宽，必须缩到 cell_w 内，不能压到下一格。
        if let Some(g) = cache.get(&fonts, '❤', Style::Regular, 1) {
            assert!(
                g.advance as usize <= fonts.cell_w,
                "窄 emoji 未缩放进格: advance={} cell_w={}",
                g.advance,
                fonts.cell_w
            );
        }
    }

    #[test]
    fn placeholder_only_for_emoji_and_private_use() {
        assert_eq!(placeholder_kind('😀'), Some(PlaceholderKind::Emoji));
        assert_eq!(placeholder_kind('❤'), Some(PlaceholderKind::Emoji));
        assert_eq!(placeholder_kind('\u{E709}'), Some(PlaceholderKind::Icon));
        assert_eq!(placeholder_kind('\u{F121}'), Some(PlaceholderKind::Icon));
        // 普通缺字和零宽码位不能画占位框。
        assert_eq!(placeholder_kind('A'), None);
        assert_eq!(placeholder_kind('你'), None);
        assert_eq!(placeholder_kind('\u{200D}'), None);
        assert_eq!(placeholder_kind('\u{FE0F}'), None);
    }

    #[test]
    fn fallback_chain_has_multiple_candidates() {
        let Some(fonts) = fonts() else { return };
        // 建立回退链（只读列表，不解析字体），确认内置链存在。
        assert!(fonts.fallback_len() >= 4, "回退链候选过少: {}", fonts.fallback_len());
    }
}
