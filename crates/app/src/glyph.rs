// SPDX-License-Identifier: GPL-3.0-only
//! 等宽字形图集：fontdue 光栅化 + 单通道 atlas 打包；彩色 emoji 走独立 RGBA 缓存。
//!
//! 只在下一次出现新字符时栅格化，之后每帧只做 coverage / RGBA 混合。
//!
//! 轮廓字体链（按顺序、全部惰性加载）：
//! 1. 主等宽字体（JetBrains Mono / DejaVu Sans Mono）；
//! 2. CJK 回退（WQY Micro Hei / Droid Sans Fallback / Noto Sans CJK / WQY Zen Hei）；
//! 3. 符号与单色 emoji（Noto Sans Symbols 2 / DejaVu Sans / Symbola / Noto Emoji）；
//! 4. 用户用 `TERVO_FALLBACK_FONTS` 追加的字体（常见用途：Nerd Font 图标）；
//! 5. 彩色位图 emoji（Noto Color Emoji 等）单独走 `ttf-parser` 读 CBDT/CBLC/sbix
//!    位图，PNG 由 `png` 解码，缩放后缓存在 [`GlyphCache`] 的 RGBA 缓存里。
//!    找不到彩色字体/字形时保持原单色回退与占位框，绝不画 tofu、也不 panic。
//!
//! emoji 的网格宽度仍由 alacritty + unicode-width 决定（通常 2 格；`❤` 这类
//! VS16 变体是 1 格），彩色位图只负责在既有格子内绘制并裁剪，不改变宽度语义。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use fontdue::{Font, FontSettings};
use owned_ttf_parser::{AsFaceRef, OwnedFace, RasterImageFormat};

pub const ATLAS_W: usize = 1024;
pub const ATLAS_H: usize = 1024;

/// 单色字形缓存上限。正常终端只有几百个可见码位；恶意/异常输出可以制造无限
/// 不同的 `(char, Style, span)`，没有上限时每个 pane 的缓存会随会话无限增长。
pub const MAX_MONO_GLYPHS: usize = 8192;
/// 彩色 emoji 缓存上限（每个条目持有解码后的 RGBA，必须比单色缓存更小）。
pub const MAX_COLOR_GLYPHS: usize = 1024;

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
    /// 只有彩色位图（CBDT/CBLC/sbix/COLR），fontdue 取不到轮廓；不进入单色轮廓链，
    /// 由 ttf-parser 彩色位图管线单独处理。
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
    // fontdue 不支持 CBDT/CBLC；这些候选项交给 ttf-parser 的彩色位图管线。
    // 常见发行版路径都列上，命中其中一个即可；不在 fontdue 轮廓链里解析。
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_COLOR,
        color_only: true,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_COLOR,
        color_only: true,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/opentype/noto/NotoColorEmoji.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_COLOR,
        color_only: true,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
        regular_index: 0,
        bold_path: None,
        bold_index: 0,
        group: GROUP_COLOR,
        color_only: true,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/google-noto-emoji/NotoColorEmoji.ttf",
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

/// 彩色 emoji 总开关：默认开启；`TERVO_COLOR_EMOJI=0/off/false` 可退回单色。
///
/// 做成环境变量而不是配置项：默认行为要彩色，排障时可一键关掉（配置 schema 不动）。
fn color_emoji_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            std::env::var("TERVO_COLOR_EMOJI").ok().as_deref(),
            Some("0" | "off" | "false" | "no")
        )
    })
}

/// 彩色位图字体的一个字形图像（借用字体文件，不复制 PNG）。
enum ColorRaster<'a> {
    Png { width: usize, height: usize, data: &'a [u8] },
    /// CBDT/EBDT 的 32 位 BGRA，通道已按 alpha 预乘（ttf-parser 文档约定）。
    BgraPremul { width: usize, height: usize, data: &'a [u8] },
}

/// 彩色位图字体：`OwnedFace` 同时持有文件字节与解析后的 `ttf-parser::Face`，
/// 文件只读一次、表只解析一次，不需要自引用 unsafe。
struct ColorFace {
    face: OwnedFace,
}

impl ColorFace {
    fn load(path: &str, index: u32) -> Option<Self> {
        let started = std::time::Instant::now();
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                if fontlog() {
                    eprintln!("[font] 彩色字体不可用 {path}#{index}: {err}");
                }
                return None;
            },
        };
        let face = match OwnedFace::from_vec(bytes, index) {
            Ok(face) => face,
            Err(err) => {
                if fontlog() {
                    eprintln!("[font] 彩色字体解析失败 {path}#{index}: {err:?}");
                }
                return None;
            },
        };
        let parsed = face.as_face_ref();
        let tables = parsed.tables();
        let has_bitmap = tables.cbdt.is_some()
            || tables.sbix.is_some()
            || tables.bdat.is_some()
            || tables.ebdt.is_some();
        let has_colr = tables.colr.is_some();
        if !has_bitmap && !has_colr {
            if fontlog() {
                eprintln!("[font] 跳过非彩色字体 {path}#{index}");
            }
            return None;
        }
        if fontlog() {
            let kind = if has_bitmap { "CBDT/CBLC/sbix" } else { "COLR" };
            eprintln!(
                "[font] 彩色字体 {path}#{index}（{kind}, {} 字形, {:?}）",
                parsed.number_of_glyphs(),
                started.elapsed()
            );
        }
        Some(Self { face })
    }

    /// 只解码 PNG 与预乘 BGRA；COLR/SVG 本轮不支持，返回 `None` 走单色回退。
    fn image(&self, ch: char, px: u16) -> Option<ColorRaster<'_>> {
        let face = self.face.as_face_ref();
        let gid = face.glyph_index(ch)?;
        let img = face.glyph_raster_image(gid, px)?;
        let (width, height) = (img.width as usize, img.height as usize);
        match img.format {
            RasterImageFormat::PNG => Some(ColorRaster::Png { width, height, data: img.data }),
            RasterImageFormat::BitmapPremulBgra32 => {
                Some(ColorRaster::BgraPremul { width, height, data: img.data })
            },
            _ => None,
        }
    }
}

/// PNG / 预乘 BGRA → 直通 alpha 的 RGBA `u32`（内存序 r | g<<8 | b<<16 | a<<24）。
///
/// 解码失败、尺寸异常或超过 1024x1024 时返回 `None`，不影响单色回退。
fn decode_color_raster(raster: &ColorRaster<'_>) -> Option<(usize, usize, Vec<u32>)> {
    const MAX_DIM: usize = 1024;
    match *raster {
        ColorRaster::Png { width, height, data } => {
            if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
                return None;
            }
            let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
            decoder.set_transformations(png::Transformations::normalize_to_color8());
            let mut reader = decoder.read_info().ok()?;
            let mut buf = vec![0u8; reader.output_buffer_size()?];
            let info = reader.next_frame(&mut buf).ok()?;
            let (w, h) = (info.width as usize, info.height as usize);
            if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                return None;
            }
            let bytes = &buf[..info.buffer_size()];
            let mut pixels = Vec::with_capacity(w * h);
            match (info.color_type, info.bit_depth) {
                (png::ColorType::Rgba, png::BitDepth::Eight) => {
                    for px in bytes.chunks_exact(4) {
                        pixels.push(
                            px[0] as u32
                                | (px[1] as u32) << 8
                                | (px[2] as u32) << 16
                                | (px[3] as u32) << 24,
                        );
                    }
                },
                (png::ColorType::Rgb, png::BitDepth::Eight) => {
                    for px in bytes.chunks_exact(3) {
                        pixels.push(
                            px[0] as u32 | (px[1] as u32) << 8 | (px[2] as u32) << 16 | 0xff00_0000,
                        );
                    }
                },
                (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
                    for px in bytes.chunks_exact(2) {
                        let v = px[0] as u32;
                        pixels.push(v | v << 8 | v << 16 | (px[1] as u32) << 24);
                    }
                },
                (png::ColorType::Grayscale, png::BitDepth::Eight) => {
                    for &v in bytes {
                        let v = v as u32;
                        pixels.push(v | v << 8 | v << 16 | 0xff00_0000);
                    }
                },
                // normalize_to_color8 会把调色板展开成 RGB/RGBA；其它位深不支持。
                _ => return None,
            }
            Some((w, h, pixels))
        },
        ColorRaster::BgraPremul { width, height, data } => {
            if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
                return None;
            }
            if data.len() < width * height * 4 {
                return None;
            }
            let mut pixels = Vec::with_capacity(width * height);
            for px in data.chunks_exact(4) {
                let a = px[3] as u32;
                let un = |v: u8| -> u32 {
                    if a == 0 { 0 } else { ((v as u32 * 255 + a / 2) / a).min(255) }
                };
                pixels.push(un(px[2]) | un(px[1]) << 8 | un(px[0]) << 16 | a << 24);
            }
            Some((width, height, pixels))
        },
    }
}

/// 等比缩放到 `max_w x max_h` 内；只缩小不放大（放大交给 Slint/显示层）。
fn fit_color_dims(src_w: usize, src_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (0, 0);
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64).min(1.0);
    let w = ((src_w as f64 * scale).round() as usize).clamp(1, max_w);
    let h = ((src_h as f64 * scale).round() as usize).clamp(1, max_h);
    (w, h)
}

/// 面积平均缩小（预乘 alpha 累加，避免透明边缘出现黑边）；尺寸相同时直接拷贝。
fn scale_color(src: &[u32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u32> {
    if dw == sw && dh == sh {
        return src.to_vec();
    }
    let mut out = vec![0u32; dw * dh];
    for dy in 0..dh {
        let sy0 = dy * sh / dh;
        let sy1 = (((dy + 1) * sh + dh - 1) / dh).max(sy0 + 1).min(sh);
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = (((dx + 1) * sw + dw - 1) / dw).max(sx0 + 1).min(sw);
            let (mut sr, mut sg, mut sb, mut sa, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let px = src[sy * sw + sx];
                    let a = (px >> 24) & 0xff;
                    sr += ((px & 0xff) as u64) * a as u64;
                    sg += (((px >> 8) & 0xff) as u64) * a as u64;
                    sb += (((px >> 16) & 0xff) as u64) * a as u64;
                    sa += a as u64;
                    n += 1;
                }
            }
            if n == 0 || sa == 0 {
                continue;
            }
            let a = (sa / n) as u32;
            let r = ((sr / sa) as u32).min(255);
            let g = ((sg / sa) as u32).min(255);
            let b = ((sb / sa) as u32).min(255);
            out[dy * dw + dx] = r | g << 8 | b << 16 | a << 24;
        }
    }
    out
}

/// 一个回退候选，三个槽位各自惰性解析；失败结果也缓存，避免逐字重试。
struct FallbackFace {
    path: String,
    index: u32,
    bold: Option<(String, u32)>,
    group: usize,
    color_only: bool,
    is_env: bool,
    regular: OnceLock<Option<Font>>,
    bold_face: OnceLock<Option<Font>>,
    /// 彩色位图字体（ttf-parser）。只有 GROUP_COLOR 与用户追加字体才会探测。
    color: OnceLock<Option<ColorFace>>,
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
            color: OnceLock::new(),
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
    ///
    /// 只有彩色位图的字体不进入轮廓链：fontdue 解析它既慢又画不出字形，
    /// 彩色绘制走 [`Self::color_face`]。
    fn load(&self, bold: bool) -> Option<&Font> {
        if self.color_only {
            return None;
        }
        if bold {
            if let Some((path, index)) = &self.bold {
                return self.load_font(path, *index, &self.bold_face);
            }
        }
        self.load_font(&self.path, self.index, &self.regular)
    }

    /// 彩色位图字体；`TERVO_FALLBACK_FONTS` 追加的字体也会在此探测一次并缓存。
    fn color_face(&self) -> Option<&ColorFace> {
        self.color
            .get_or_init(|| {
                if fontlog() {
                    eprintln!("[font] 探测彩色字体 {}#{}", self.path, self.index);
                }
                ColorFace::load(&self.path, self.index)
            })
            .as_ref()
    }
}

struct Fallback {
    faces: Vec<FallbackFace>,
    /// 每个用途组的候选下标；`glyph_face` 按字符类型只走相关组。
    groups: [Vec<usize>; GROUP_COUNT],
    /// 彩色位图候选：用户追加字体优先，然后内置 COLOR 组。只探测这些，不碰 CJK。
    color_candidates: Vec<usize>,
}

/// 不同字符类型对应的回退组顺序（只含轮廓字体；彩色位图走独立管线）。
///
/// - 中文：CJK → 符号 → 单色 emoji（不碰图标组，Nerd 字体不该抢中文）
/// - emoji：符号（DejaVu 单色）→ 单色 emoji；**不扫 CJK、不解析彩色字体**
/// - PUA/Nerd 图标：图标 → 符号；没有 Nerd 字体时直接占位，不去解析 18 MB CJK
/// - 其它：符号 → CJK → 单色 emoji
fn group_order(ch: char) -> &'static [usize] {
    use PlaceholderKind::*;
    match placeholder_kind(ch) {
        Some(Emoji) => &[GROUP_SYMBOL, GROUP_EMOJI],
        Some(Icon) => &[GROUP_ICON, GROUP_SYMBOL],
        None if is_cjk(ch) => &[GROUP_CJK, GROUP_SYMBOL, GROUP_EMOJI, GROUP_ICON],
        None => &[GROUP_SYMBOL, GROUP_CJK, GROUP_EMOJI, GROUP_ICON],
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

    /// 第 `skip` 个**声称覆盖** `ch` 的轮廓字体：0 = 主字体，其后按回退链顺序。
    ///
    /// 只有彩色位图的字体不在这里出现（[`FallbackFace::load`] 直接返回 None）；
    /// emoji 的彩色绘制走 [`Self::color_image`]，两者都不需要为 emoji 解析 CJK 链。
    pub fn glyph_face(&self, ch: char, style: Style, mut skip: usize) -> Option<&Font> {
        let primary = &self.faces[style.idx()];
        if primary.has_glyph(ch) {
            if skip == 0 {
                return Some(primary);
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
                        return Some(font);
                    }
                    skip -= 1;
                }
            }
        }
        None
    }

    /// 取 emoji 的彩色位图（PNG / 预乘 BGRA 原始数据）。
    ///
    /// 只在 emoji 码位调用；候选只包含 COLOR 组与 `TERVO_FALLBACK_FONTS`，
    /// 不会碰 CJK 字体。字体探测结果（含失败）缓存在回退链里。
    fn color_image(&self, ch: char, px: u16) -> Option<ColorRaster<'_>> {
        if !color_emoji_enabled() || !is_emoji(ch as u32) {
            return None;
        }
        let fallback = self.fallback();
        for &i in &fallback.color_candidates {
            let Some(face) = fallback.faces[i].color_face() else { continue };
            if let Some(image) = face.image(ch, px) {
                return Some(image);
            }
        }
        None
    }

    /// 字体链里是否有**轮廓**字体覆盖该字符（用于测试/诊断；不含彩色位图字体）。
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

    /// 彩色位图链里是否有该字符（诊断/测试用；会触发彩色候选探测）。
    #[allow(dead_code)]
    pub fn has_color_glyph(&self, ch: char) -> bool {
        self.color_image(ch, 16).is_some()
    }

    /// 字体链建立后的候选数量（诊断/测试用）。
    #[allow(dead_code)]
    pub fn fallback_len(&self) -> usize {
        self.fallback().faces.len()
    }

    /// 回退链是否已经建立（诊断/测试用）。
    #[allow(dead_code)]
    pub fn fallback_initialized(&self) -> bool {
        self.fallback.get().is_some()
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
    // CJK 回退字体优先按文件大小升序：fontdue 解析一个 18 MB TTC 会常驻上百 MB，
    // 先用 3.8 MB 的 Droid 覆盖绝大多数汉字，避免为个别生僻字直接解析最大字体。
    specs.sort_by_key(|s| {
        if s.group == GROUP_CJK {
            std::fs::metadata(&s.path).map(|m| m.len()).unwrap_or(u64::MAX)
        } else {
            0
        }
    });
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
    // 追加字体也会作为彩色候选探测一次（支持用户指定自带彩色 emoji 字体）。
    let mut color_candidates = env_indices.clone();
    groups[GROUP_SYMBOL] = env_indices;
    for (i, face) in faces.iter().enumerate() {
        if !face.is_env {
            groups[face.group].push(i);
            if face.group == GROUP_COLOR {
                color_candidates.push(i);
            }
        }
    }
    if fontlog() {
        eprintln!(
            "[font] 回退链 {} 个候选（按需解析；彩色候选 {} 个）",
            faces.len(),
            color_candidates.len()
        );
    }
    Fallback { faces, groups, color_candidates }
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

/// 几何图形区里 Unicode 默认呈现为**文本**的 Emoji（`Emoji_Presentation=No`）。
///
/// Noto Color Emoji 也收了这个区里的几个码位，且画成橙色媒体按钮：
/// dsh-tui 侧栏的 ◀/▶ 翻页键（U+25C0 / U+25B6）因此会变成橙方块，把
/// JetBrains Mono / DejaVu 本来能画的单色三角盖掉。终端惯例是：没有
/// VS16（U+FE0F）时按文本呈现走字体轮廓，VS16 才请求彩色 emoji。
///
/// 这里只列 0x25A0..=0x25FF 中 `Emoji_Presentation=No` 的六个码位；
/// U+25FD/U+25FE 默认就是 emoji 呈现，仍走彩色路径。其它区段（❤ 等）
/// 的既有彩色行为不动，避免扩大改动面。
fn is_text_presentation_geometric(ch: char) -> bool {
    matches!(ch as u32, 0x25AA | 0x25AB | 0x25B6 | 0x25C0 | 0x25FB | 0x25FC)
}

/// 该格是否应按彩色 emoji 位图绘制。
///
/// `marks` 是挂在该格上的零宽码位（变体选择符等）。普通 emoji 保持
/// “彩色优先、单色兜底”的既有行为；文本默认的几何图形只有在 VS16 明确
/// 请求 emoji 呈现时才查彩色字体。ASCII/CJK 永远不会触发彩色探测。
pub fn prefers_color_emoji(ch: char, marks: &[char]) -> bool {
    if placeholder_kind(ch) != Some(PlaceholderKind::Emoji) {
        return false;
    }
    !is_text_presentation_geometric(ch) || marks.contains(&'\u{FE0F}')
}

/// 已解码并缩放好的彩色 emoji 位图。
///
/// 像素是直通 alpha 的 RGBA（内存序 r | g<<8 | b<<16 | a<<24）；`Rc` 让缓存命中
/// 只复制一个指针，不重新解码/缩放。
#[derive(Clone, Debug)]
pub struct ColorGlyph {
    pub w: u16,
    pub h: u16,
    /// 位图左边相对单元格左边的像素偏移
    pub x_off: i16,
    /// 位图顶边相对单元格顶边的像素偏移
    pub y_off: i16,
    pixels: Rc<Vec<u32>>,
}

impl ColorGlyph {
    #[inline]
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
}

pub struct GlyphCache {
    atlas: Vec<u8>,
    pen_x: usize,
    pen_y: usize,
    row_h: usize,
    /// (字符, 样式, 占格数)：同一字符可能在 1 格/2 格上下文里缩放不同。
    map: HashMap<(char, Style, u8), Option<Glyph>>,
    /// 彩色 emoji 独立缓存：不占单通道 atlas，命中不重复读字体/解 PNG/缩放。
    /// 键不含样式：彩色位图没有字重/斜体，粗体 emoji 复用同一份像素。
    color_map: HashMap<(char, u8), Option<ColorGlyph>>,
    pub rasterized: u64,
    pub atlas_full: u64,
    pub color_decodes: u64,
    pub color_hits: u64,
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self {
            atlas: vec![0u8; ATLAS_W * ATLAS_H],
            pen_x: 0,
            pen_y: 0,
            row_h: 0,
            map: HashMap::with_capacity(512),
            color_map: HashMap::with_capacity(128),
            rasterized: 0,
            atlas_full: 0,
            color_decodes: 0,
            color_hits: 0,
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
        self.enforce_limits();
        glyph
    }

    /// 缓存达到上限时整体丢弃并复用 atlas，而不是让内存随码位种类线性增长。
    ///
    /// 丢弃只会让后续字形多一次栅格化，不影响当前帧已经画出的像素；正常会话
    /// 远达不到上限，因此不会触发。
    fn enforce_limits(&mut self) {
        if self.map.len() > MAX_MONO_GLYPHS {
            self.map.clear();
            self.atlas.fill(0);
            self.pen_x = 0;
            self.pen_y = 0;
            self.row_h = 0;
        }
        if self.color_map.len() > MAX_COLOR_GLYPHS {
            self.color_map.clear();
        }
    }

    /// 取彩色 emoji 位图；`span` 与单色字形一致（通常 2 格，`❤`+VS16 为 1 格）。
    ///
    /// 只对 emoji 码位查彩色字体，CJK/ASCII 完全不会触碰彩色候选。命中缓存时
    /// 不读文件、不解析字体、不重复解 PNG；失败结果也缓存（不会逐帧重试）。
    pub fn get_color(
        &mut self,
        fonts: &FontSet,
        ch: char,
        _style: Style,
        span: usize,
    ) -> Option<ColorGlyph> {
        if !color_emoji_enabled() || !is_emoji(ch as u32) {
            return None;
        }
        let span = span.clamp(1, u8::MAX as usize) as u8;
        let key = (ch, span);
        if let Some(glyph) = self.color_map.get(&key) {
            self.color_hits += 1;
            return glyph.clone();
        }
        let glyph = self.rasterize_color(fonts, ch, span as usize);
        if glyph.is_some() {
            self.color_decodes += 1;
        }
        self.color_map.insert(key, glyph.clone());
        self.enforce_limits();
        glyph
    }

    fn rasterize_color(&mut self, fonts: &FontSet, ch: char, span: usize) -> Option<ColorGlyph> {
        // 彩色 emoji 没有字重/斜体；用字体自身的 px 选最大 strike（Noto 只有一档）。
        let px = fonts.size.round().clamp(1.0, u16::MAX as f32) as u16;
        let raster = fonts.color_image(ch, px)?;
        let (src_w, src_h, src) = decode_color_raster(&raster)?;
        if src.is_empty() {
            return None;
        }
        let max_w = span.max(1) * fonts.cell_w;
        let max_h = fonts.cell_h.max(1);
        let (w, h) = fit_color_dims(src_w, src_h, max_w, max_h);
        if w == 0 || h == 0 {
            return None;
        }
        let pixels = scale_color(&src, src_w, src_h, w, h);
        Some(ColorGlyph {
            w: w as u16,
            h: h as u16,
            x_off: ((max_w - w) / 2) as i16,
            // 垂直居中：emoji 位图通常带内边距，贴基线会显得偏下。
            y_off: ((max_h - h) / 2) as i16,
            pixels: Rc::new(pixels),
        })
    }

    fn rasterize(&mut self, fonts: &FontSet, ch: char, style: Style, span: usize) -> Option<Glyph> {
        let limit = (span.max(1) * fonts.cell_w) as f32;
        let mut skip = 0usize;
        // 依次尝试主字体 + 回退链里所有声称覆盖该字符的轮廓字体；彩色位图字体
        // 不在这个链里（由 get_color 单独处理），所以这里不会被 10 MB emoji 字体拖慢。
        while let Some(font) = fonts.glyph_face(ch, style, skip) {
            skip += 1;
            let Some((metrics, bitmap)) = rasterize_fit(font, ch, fonts.size, limit) else {
                continue;
            };
            if let Some(glyph) = self.insert_bitmap(metrics, &bitmap) {
                return Some(glyph);
            }
            // atlas 满：继续换下一个字体也放不下，直接放弃本帧。
            return None;
        }
        None
    }

    /// 把一张已栅格化的位图写进 atlas，返回可直接绘制的字形坐标。
    fn insert_bitmap(&mut self, metrics: fontdue::Metrics, bitmap: &[u8]) -> Option<Glyph> {
        let (w, h) = (metrics.width, metrics.height);
        if w + 2 > ATLAS_W || h + 2 > ATLAS_H {
            return None;
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
        Some(glyph)
    }

    /// 组合标记专用：只在主字体里取字形，**不建立/不探测回退链**。
    ///
    /// 组合音标、keycap(U+20E3) 等零宽标记在多数回退字体里也没有轮廓；若走
    /// `get` 的完整回退链，会为了一个标记按需解析 CJK/emoji 字体（fontdue
    /// 解析单个 CJK 字体可达数百 MB），这是长时内存暴涨的主要来源之一。
    pub fn get_mark(
        &mut self,
        fonts: &FontSet,
        ch: char,
        style: Style,
        span: usize,
    ) -> Option<Glyph> {
        let span = span.clamp(1, u8::MAX as usize) as u8;
        let key = (ch, style, span);
        if let Some(g) = self.map.get(&key) {
            return *g;
        }
        let limit = (span.max(1) as usize * fonts.cell_w) as f32;
        let font = &fonts.faces[style.idx()];
        let glyph = rasterize_fit(font, ch, fonts.size, limit)
            .and_then(|(metrics, bitmap)| self.insert_bitmap(metrics, &bitmap));
        self.map.insert(key, glyph);
        self.enforce_limits();
        glyph
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
        // cmap 有该码位但没有可轮廓化的字形（位图字体等）。
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
    fn color_emoji_decodes_cbdt_and_caches_after_first_time() {
        let Some(fonts) = fonts() else { return };
        if !fonts.has_color_glyph('😀') {
            eprintln!("跳过：系统没有 CBDT/CBLC/sbix 彩色 emoji 字体");
            return;
        }
        let mut cache = GlyphCache::default();
        let g = cache.get_color(&fonts, '😀', Style::Regular, 2).expect("😀 应能解出彩色位图");
        assert!(g.w >= 8 && g.h >= 8, "彩色 emoji 位图过小: {g:?}");
        assert!(
            g.w as usize <= 2 * fonts.cell_w && g.h as usize <= fonts.cell_h,
            "彩色 emoji 超出两格/行高: {g:?} cell={}x{}",
            fonts.cell_w,
            fonts.cell_h
        );
        assert!(colored_pixels(&g) > 10, "彩色 emoji 没有彩色像素（解码成了灰度？）");
        assert_eq!(cache.color_decodes, 1);

        // 彩色管线只探测 COLOR/env 候选，不能为了 emoji 解析 CJK 字体。
        let fallback = fonts.fallback.get().expect("彩色查询应已建立回退链");
        assert!(!fallback.color_candidates.is_empty(), "没有彩色候选");
        for face in &fallback.faces {
            if face.group == GROUP_CJK {
                assert!(
                    face.regular.get().is_none(),
                    "emoji 彩色渲染解析了 CJK 字体: {}",
                    face.path
                );
            }
        }

        // 缓存命中：不重新读字体、不重复解 PNG；Rc 指向同一块像素。
        let again = cache.get_color(&fonts, '😀', Style::Regular, 2).expect("缓存命中");
        assert_eq!(cache.color_decodes, 1, "缓存命中不应重复解码 PNG");
        assert!(cache.color_hits >= 1, "缓存命中计数未增加");
        assert!(std::rc::Rc::ptr_eq(&g.pixels, &again.pixels), "缓存命中应复用同一 RGBA 缓冲");
        assert_eq!((g.w, g.h), (again.w, again.h));

        // 彩色位图没有字重：粗体 emoji 复用同一份像素，不重复解码。
        let bold = cache.get_color(&fonts, '😀', Style::Bold, 2).expect("粗体复用彩色位图");
        assert_eq!(cache.color_decodes, 1, "粗体 emoji 不应重复解码");
        assert!(std::rc::Rc::ptr_eq(&g.pixels, &bold.pixels), "粗体应复用同一 RGBA 缓冲");
    }

    #[test]
    fn color_emoji_narrow_heart_fits_one_cell() {
        let Some(fonts) = fonts() else { return };
        if !fonts.has_color_glyph('❤') {
            eprintln!("跳过：系统没有 ❤ 的彩色位图");
            return;
        }
        let mut cache = GlyphCache::default();
        let g = cache.get_color(&fonts, '❤', Style::Regular, 1).expect("❤ 彩色位图");
        assert!(g.w as usize <= fonts.cell_w, "1 格彩色 emoji 宽度越界: {g:?}");
        assert!(g.h as usize <= fonts.cell_h, "1 格彩色 emoji 高度越界: {g:?}");
        assert!(colored_pixels(&g) > 0, "❤ 没有彩色像素");
    }

    #[test]
    fn color_emoji_is_not_probed_for_ascii_or_cjk() {
        let Some(fonts) = fonts() else { return };
        let mut cache = GlyphCache::default();
        // 普通字符/中文不能触发彩色候选探测，更不能为 emoji 解析 CJK 字体。
        assert!(cache.get_color(&fonts, 'A', Style::Regular, 1).is_none());
        assert!(cache.get_color(&fonts, '你', Style::Regular, 2).is_none());
        assert!(!fonts.fallback_initialized(), "ASCII/CJK 不应建立彩色回退链");
        assert_eq!(cache.color_decodes, 0);
    }

    #[test]
    fn glyph_cache_is_bounded_by_explicit_limits() {
        let mut cache = GlyphCache::default();
        for i in 0..(MAX_MONO_GLYPHS + 50) {
            let ch = char::from_u32(0xE000 + i as u32).expect("private use char");
            cache.map.insert((ch, Style::Regular, 1), None);
            cache.enforce_limits();
        }
        assert!(
            cache.map.len() <= MAX_MONO_GLYPHS,
            "单色字形缓存超上限: {} > {}",
            cache.map.len(),
            MAX_MONO_GLYPHS
        );
        assert_eq!((cache.pen_x, cache.pen_y), (0, 0), "清空缓存应同时重置 atlas 游标");

        for i in 0..(MAX_COLOR_GLYPHS + 10) {
            let ch = char::from_u32(0x1F300 + i as u32).expect("emoji char");
            cache.color_map.insert((ch, 2), None);
            cache.enforce_limits();
        }
        assert!(
            cache.color_map.len() <= MAX_COLOR_GLYPHS,
            "彩色 emoji 缓存超上限: {} > {}",
            cache.color_map.len(),
            MAX_COLOR_GLYPHS
        );
    }

    #[test]
    fn combining_mark_never_builds_fallback_chain() {
        let Some(fonts) = fonts() else { return };
        assert!(!fonts.fallback_initialized(), "初始不应建立回退链");
        let mut cache = GlyphCache::default();
        // keycap U+20E3 在多数回退字体里没有轮廓；不允许为它解析整套字体。
        let _ = cache.get_mark(&fonts, '\u{20E3}', Style::Regular, 1);
        assert!(
            !fonts.fallback_initialized(),
            "组合标记不应触发回退字体解析（fontdue 解析 CJK 字体可达数百 MB）"
        );
    }

    #[test]
    fn color_font_loader_rejects_non_color_and_missing_files() {
        // 主字体只有轮廓，不能被当成彩色字体。
        assert!(ColorFace::load(FONT_SETS[0].regular, 0).is_none());
        assert!(ColorFace::load("/nonexistent/NotoColorEmoji.ttf", 0).is_none());
    }

    #[test]
    fn color_raster_decode_handles_premultiplied_bgra() {
        // 半透明纯红：预乘 BGRA = B0 G0 R128 A128 → 直通 RGBA R255。
        let data = [0u8, 0, 128, 128];
        let raster = ColorRaster::BgraPremul { width: 1, height: 1, data: &data };
        let (w, h, px) = decode_color_raster(&raster).expect("预乘 BGRA 应能解码");
        assert_eq!((w, h), (1, 1));
        assert_eq!(px[0] & 0xff, 255, "红通道未反预乘: {:#x}", px[0]);
        assert_eq!(px[0] >> 24, 128, "alpha 被改坏: {:#x}", px[0]);

        // 损坏 PNG / 0 尺寸不能 panic。
        let bad = ColorRaster::Png { width: 4, height: 4, data: b"not a png" };
        assert!(decode_color_raster(&bad).is_none());
        let empty = ColorRaster::BgraPremul { width: 0, height: 0, data: &[] };
        assert!(decode_color_raster(&empty).is_none());
    }

    #[test]
    fn color_scaling_fits_grid_and_keeps_color() {
        let red = 0xffu32 | 0x00 << 8 | 0x00 << 16 | 0xff << 24;
        let src = vec![red; 4];
        let out = scale_color(&src, 2, 2, 1, 1);
        assert_eq!(out, vec![red], "缩小后颜色/alpha 改变");

        let (w, h) = fit_color_dims(136, 128, 18, 20);
        assert!(w > 0 && h > 0 && w <= 18 && h <= 20, "缩放尺寸越界: {w}x{h}");
        assert_eq!(fit_color_dims(0, 128, 18, 20), (0, 0), "0 宽源图应拒绝");

        // 全透明源图不能除零/panic。
        let transparent = vec![0u32; 2];
        let out = scale_color(&transparent, 2, 1, 1, 1);
        assert_eq!(out, vec![0]);
    }

    fn colored_pixels(g: &ColorGlyph) -> usize {
        g.pixels()
            .iter()
            .filter(|p| {
                let a = *p >> 24 & 0xff;
                let r = *p & 0xff;
                let gg = (*p >> 8) & 0xff;
                let b = (*p >> 16) & 0xff;
                a > 128 && (r.abs_diff(gg) > 8 || gg.abs_diff(b) > 8 || r.abs_diff(b) > 8)
            })
            .count()
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
    fn text_default_geometric_arrows_stay_monochrome_without_vs16() {
        // dsh-tui PanelBar 用 U+25C0/U+25B6 画侧栏翻页键，输出不带 VS16。
        assert!(!prefers_color_emoji('\u{25C0}', &[]));
        assert!(!prefers_color_emoji('\u{25B6}', &[]));
        assert!(!prefers_color_emoji('\u{25AA}', &[]));
        assert!(!prefers_color_emoji('\u{25FB}', &[]));
        // VS16 明确请求 emoji 呈现时仍走彩色位图。
        assert!(prefers_color_emoji('\u{25C0}', &['\u{FE0F}']));
        assert!(prefers_color_emoji('\u{25B6}', &['\u{FE0F}']));
        // 默认 emoji 呈现的几何图形、普通 emoji 保持彩色优先。
        assert!(prefers_color_emoji('\u{25FD}', &[]));
        assert!(prefers_color_emoji('\u{25FE}', &[]));
        assert!(prefers_color_emoji('😀', &[]));
        // 非 emoji 文本（含缺字 ⤢）不进入彩色探测。
        assert!(!prefers_color_emoji('<', &[]));
        assert!(!prefers_color_emoji('你', &[]));
        assert!(!prefers_color_emoji('\u{2922}', &[]));
    }

    #[test]
    fn fallback_chain_has_multiple_candidates() {
        let Some(fonts) = fonts() else { return };
        // 建立回退链（只读列表，不解析字体），确认内置链存在。
        assert!(fonts.fallback_len() >= 4, "回退链候选过少: {}", fonts.fallback_len());
    }
}
