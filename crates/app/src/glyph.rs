//! 等宽字形图集：fontdue 光栅化 + 单通道 atlas 打包。
//!
//! 只在下一次出现新字符时栅格化，之后每帧只做 coverage 混合。

use std::collections::HashMap;

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

/// CJK / 符号回退字体。TTC 需要指定 face index：
/// Noto Sans (Mono) CJK SC 在 Regular/Bold TTC 里都是第 7 个 face。
struct FallbackCandidate {
    regular: &'static str,
    regular_index: u32,
    bold: &'static str,
    bold_index: u32,
}

const FALLBACK_SETS: &[FallbackCandidate] = &[
    // 4.9 MB / ~106 ms：日常首选，启动开销可接受
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        regular_index: 0,
        bold: "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        bold_index: 0,
    },
    // 3.8 MB / ~70 ms：无粗体、无拉丁，但覆盖全，作为轻量兜底
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        regular_index: 0,
        bold: "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        bold_index: 0,
    },
    // 18 MB / ~260-335 ms：最好看，但只在没有轻量字体时才用
    FallbackCandidate {
        regular: "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        regular_index: 7, // Noto Sans Mono CJK SC
        bold: "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
        bold_index: 7,
    },
    FallbackCandidate {
        regular: "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        regular_index: 1, // WenQuanYi Zen Hei Mono
        bold: "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        bold_index: 1,
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

pub struct FontSet {
    faces: [Font; 4],
    /// CJK 回退：0 = Regular，1 = Bold（字体本身没粗体时复用 Regular）
    fallback: [Option<Font>; 2],
    pub size: f32,
    pub cell_w: usize,
    pub cell_h: usize,
    /// 行顶部到基线的像素距离
    pub baseline: i32,
    #[allow(dead_code)]
    pub font_path: String,
    #[allow(dead_code)]
    pub fallback_path: Option<String>,
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

            let (fallback, fallback_path) = load_fallback();

            return Some(Self {
                faces: [regular, bold, italic, bold_italic],
                fallback,
                size,
                cell_w,
                cell_h,
                baseline,
                font_path: cand.regular.to_string(),
                fallback_path,
            });
        }
        None
    }

    #[allow(dead_code)]
    #[inline]
    pub fn face(&self, style: Style) -> &Font {
        &self.faces[style.idx()]
    }

    /// 优先主字体；主字体缺字（典型：中文）时退到 CJK 字体。
    pub fn face_for(&self, ch: char, style: Style) -> &Font {
        let primary = &self.faces[style.idx()];
        if primary.has_glyph(ch) {
            return primary;
        }
        let slot = usize::from(matches!(style, Style::Bold | Style::BoldItalic));
        // 回退字体没有粗体（例如 Micro Hei）时用常规体顶替，绝不能退回主字体画 tofu。
        let fallback = self.fallback[slot].as_ref().or(self.fallback[0].as_ref());
        match fallback {
            Some(f) if f.has_glyph(ch) => f,
            _ => primary,
        }
    }

    /// 主字体 + 回退字体里是否有该字符（用于跳过 emoji 等完全缺失的字形）。
    pub fn has_glyph(&self, ch: char) -> bool {
        self.faces.iter().any(|f| f.has_glyph(ch))
            || self.fallback.iter().flatten().any(|f| f.has_glyph(ch))
    }
}

fn load_fallback() -> ([Option<Font>; 2], Option<String>) {
    for cand in FALLBACK_SETS {
        let t = std::time::Instant::now();
        let Some(regular) = load_font_at(cand.regular, cand.regular_index) else {
            if std::env::var_os("RUSTTABBY_FONTLOG").is_some() {
                eprintln!("[font] 回退失败 {}#{} ({:?})", cand.regular, cand.regular_index, t.elapsed());
            }
            continue;
        };
        let bold = if cand.bold == cand.regular && cand.bold_index == cand.regular_index {
            None
        } else {
            load_font_at(cand.bold, cand.bold_index)
        };
        let path = format!("{}#{}", cand.regular, cand.regular_index);
        if std::env::var_os("RUSTTABBY_FONTLOG").is_some() {
            eprintln!("[font] 回退 {path} ({:?})", t.elapsed());
        }
        return ([Some(regular), bold], Some(path));
    }
    ([None, None], None)
}

pub struct GlyphCache {
    atlas: Vec<u8>,
    pen_x: usize,
    pen_y: usize,
    row_h: usize,
    map: HashMap<(char, Style), Option<Glyph>>,
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

    pub fn get(&mut self, fonts: &FontSet, ch: char, style: Style) -> Option<Glyph> {
        if let Some(g) = self.map.get(&(ch, style)) {
            return *g;
        }
        let glyph = self.rasterize(fonts, ch, style);
        self.map.insert((ch, style), glyph);
        glyph
    }

    fn rasterize(&mut self, fonts: &FontSet, ch: char, style: Style) -> Option<Glyph> {
        // 主字体与回退字体都没有这个码位（emoji 等）时直接留空，避免画出 tofu。
        if !fonts.has_glyph(ch) {
            return None;
        }
        let font = fonts.face_for(ch, style);
        let (metrics, bitmap) = font.rasterize(ch, fonts.size);
        self.rasterized += 1;
        if metrics.width == 0 || metrics.height == 0 {
            return None;
        }
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
        Some(glyph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_fallback_rasterizes() {
        let Some(fonts) = FontSet::load(15.0) else {
            eprintln!("跳过：没有可用字体");
            return;
        };
        // 主字体 JetBrains Mono 没有中文字形，必须由回退字体接住。
        assert!(fonts.face(Style::Regular).has_glyph('A'));
        assert!(!fonts.face(Style::Regular).has_glyph('你'), "预期主字体缺中文");
        assert!(fonts.has_glyph('你'));
        assert!(fonts.has_glyph('好'));

        let mut cache = GlyphCache::default();
        let g = cache.get(&fonts, '你', Style::Regular).expect("中文应能栅格化");
        assert!(g.w >= 8 && g.h >= 8, "中文位图过小: {g:?}");
        assert!(g.advance >= 10, "中文步进异常: {g:?}");
        // 回退字体没有粗体时也必须能画（用常规体顶替，不能掉回主字体变 tofu）
        assert!(cache.get(&fonts, '你', Style::Bold).is_some(), "粗体中文丢字形");
        assert!(cache.get(&fonts, '好', Style::Italic).is_some(), "斜体中文丢字形");
    }

    #[test]
    fn latin_uses_primary_face() {
        let Some(fonts) = FontSet::load(15.0) else { return };
        let mut cache = GlyphCache::default();
        let g = cache.get(&fonts, 'M', Style::Regular).unwrap();
        // cell 宽就是按 'M' 的步进算的
        assert_eq!(g.advance as usize, fonts.cell_w);
    }
}
