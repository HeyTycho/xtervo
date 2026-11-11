//! CPU-side glyph rasterization and a GPU-resident coverage atlas.
//!
//! The atlas is a single `R8Unorm` texture. Terminal glyphs are masks, not
//! colored images: storing one byte of coverage per texel is enough, and the
//! foreground color travels per instance. At 1024x1024 that is 1 MiB, while an
//! RGBA atlas would be 4 MiB with three unused channels.
//!
//! Packing is a simple shelf allocator. There is no eviction in this prototype:
//! when the shelf runs out of vertical space [`GlyphAtlas::ensure`] returns
//! [`AtlasError::Full`] and the frame is rejected before a render pass starts.
//! That failure mode is covered by tests; silently dropping glyphs is not
//! allowed.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use fontdue::{Font, FontSettings};
use owned_ttf_parser::{AsFaceRef, OwnedFace};

use tervo_scene::TextAttributes;

/// Format of the glyph coverage atlas.
pub const ATLAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Default atlas edge length. 1024x1024 R8 = 1 MiB of GPU memory.
pub const DEFAULT_ATLAS_SIZE: u32 = 1024;

/// Default number of empty texels between packed glyphs.
pub const DEFAULT_PADDING: u32 = 1;

/// Font face selected by [`TextAttributes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FontStyle {
    #[default]
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl FontStyle {
    /// Resolves the style requested by terminal cell attributes.
    ///
    /// `dim` maps to the bold face, matching the legacy CPU renderer's
    /// `style_of` (`bold || dim`).
    pub fn from_attributes(attributes: TextAttributes) -> Self {
        match (attributes.bold || attributes.dim, attributes.italic) {
            (false, false) => Self::Regular,
            (true, false) => Self::Bold,
            (false, true) => Self::Italic,
            (true, true) => Self::BoldItalic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::BoldItalic => "bold-italic",
        }
    }
}

/// Monospace metrics used to lay out one cell at a given pixel size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
    /// Distance from the top of the cell to the text baseline.
    pub baseline: f32,
    pub ascent: f32,
    pub descent: f32,
}

/// Atlas construction parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasConfig {
    pub width: u32,
    pub height: u32,
    pub padding: u32,
}

impl Default for AtlasConfig {
    fn default() -> Self {
        Self {
            width: DEFAULT_ATLAS_SIZE,
            height: DEFAULT_ATLAS_SIZE,
            padding: DEFAULT_PADDING,
        }
    }
}

impl AtlasConfig {
    /// Small explicitly-sized atlas, primarily for failure-mode tests.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            padding: DEFAULT_PADDING,
        }
    }
}

/// One cached glyph inside the atlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphSlot {
    /// Left edge of the coverage bitmap in atlas texels.
    pub x: u32,
    /// Top edge of the coverage bitmap in atlas texels.
    pub y: u32,
    /// Coverage bitmap width in texels.
    pub w: u32,
    /// Coverage bitmap height in texels.
    pub h: u32,
    /// Whole-pixel horizontal offset from the pen position.
    pub xmin: i32,
    /// Whole-pixel offset from the baseline to the bitmap bottom.
    pub ymin: i32,
    /// Rounded advance width of the face that produced this glyph.
    ///
    /// Wide/fallback glyphs are centered inside their cell span exactly like
    /// the CPU renderer, which needs the advance even when the bitmap is empty.
    pub advance: i32,
}

impl GlyphSlot {
    /// Whether this glyph has no drawable coverage (space, or a missing outline).
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Atlas UV rectangle as `[u0, v0, u1, v1]`.
    pub fn uv(&self, atlas_width: u32, atlas_height: u32) -> [f32; 4] {
        let aw = atlas_width.max(1) as f32;
        let ah = atlas_height.max(1) as f32;
        [
            self.x as f32 / aw,
            self.y as f32 / ah,
            (self.x + self.w) as f32 / aw,
            (self.y + self.h) as f32 / ah,
        ]
    }
}

/// Result of one atlas lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphLookup {
    /// The coverage slot, or `None` for whitespace/zero-size outlines.
    pub slot: Option<GlyphSlot>,
    /// The requested bold/italic face was unavailable and regular was used.
    pub style_fallback: bool,
    /// The primary face had no glyph and a fallback font was used.
    pub glyph_fallback: bool,
    /// No loaded font covers this character; renderers must draw an explicit
    /// fallback marker and count it instead of silently dropping/misplacing it.
    pub missing: bool,
}

/// Cumulative atlas counters. Per-frame deltas are produced by the renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtlasStats {
    pub lookups: u64,
    pub hits: u64,
    pub misses: u64,
    pub rasterized: u64,
    /// Number of `queue.write_texture` calls for newly rasterized glyphs.
    pub uploads: u64,
    /// Bytes uploaded by those calls. This is glyph-sized, never full-screen.
    pub uploaded_bytes: u64,
    /// Glyphs whose fontdue bitmap was empty (whitespace etc.).
    pub empty_glyphs: u64,
    /// Lookups that requested a style face which was not loaded.
    pub style_fallbacks: u64,
    /// Glyphs rasterized from a fallback font because the primary face lacked
    /// the character (CJK/symbol/emoji outline chain).
    pub glyph_fallbacks: u64,
    /// Fallback font files parsed on demand.
    pub fallback_fonts_loaded: u64,
    /// Characters no loaded font covers. The renderer draws a deterministic
    /// `?` marker and counts them; this is never a silent drop.
    pub missing_glyphs: u64,
    /// Frames/glyphs rejected because the shelf allocator could not fit them.
    pub full_errors: u64,
}

/// Failures while loading a font, creating the atlas, or packing a glyph.
#[derive(Debug)]
pub enum AtlasError {
    /// No usable monospace font was found.
    FontNotFound { tried: Vec<PathBuf> },
    /// A font file could not be read.
    FontIo {
        path: PathBuf,
        source: std::io::Error,
    },
    /// fontdue rejected the file.
    FontParse { path: PathBuf, message: String },
    /// Atlas dimensions are invalid for the device.
    InvalidConfig { width: u32, height: u32, max: u32 },
    /// A single glyph cannot fit even in an empty atlas.
    GlyphTooLarge {
        ch: char,
        style: FontStyle,
        px: u16,
        width: u32,
        height: u32,
        atlas_width: u32,
        atlas_height: u32,
    },
    /// The shelf allocator ran out of vertical space.
    Full {
        ch: char,
        style: FontStyle,
        px: u16,
        width: u32,
        height: u32,
        atlas_width: u32,
        atlas_height: u32,
    },
}

impl fmt::Display for AtlasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FontNotFound { tried } => {
                write!(f, "no monospace font found; tried: ")?;
                for (index, path) in tried.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", path.display())?;
                }
                f.write_str(" (set TERVO_WGPU_FONT=/path/to/font.ttf)")
            }
            Self::FontIo { path, source } => {
                write!(f, "failed to read font {}: {source}", path.display())
            }
            Self::FontParse { path, message } => {
                write!(f, "failed to parse font {}: {message}", path.display())
            }
            Self::InvalidConfig { width, height, max } => write!(
                f,
                "atlas {width}x{height} is invalid (edges must be 1..={max},                  padding must be smaller than both edges)"
            ),
            Self::GlyphTooLarge {
                ch,
                style,
                px,
                width,
                height,
                atlas_width,
                atlas_height,
            } => write!(
                f,
                "glyph {ch:?} ({}, {px}px) rasterizes to {width}x{height}, \
                 larger than the {atlas_width}x{atlas_height} atlas",
                style.label()
            ),
            Self::Full {
                ch,
                style,
                px,
                width,
                height,
                atlas_width,
                atlas_height,
            } => write!(
                f,
                "glyph atlas full: {ch:?} ({}, {px}px, {width}x{height}) \
                 does not fit in {atlas_width}x{atlas_height}; \
                 increase AtlasConfig or reduce the number of sizes",
                style.label()
            ),
        }
    }
}

impl std::error::Error for AtlasError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FontIo { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Reads a font file and parses it with fontdue.
fn load_font_at(path: &Path, collection_index: u32) -> Result<Font, AtlasError> {
    let bytes = std::fs::read(path).map_err(|source| AtlasError::FontIo {
        path: path.to_path_buf(),
        source,
    })?;
    let settings = FontSettings {
        collection_index,
        ..FontSettings::default()
    };
    Font::from_bytes(bytes, settings).map_err(|message| AtlasError::FontParse {
        path: path.to_path_buf(),
        message: message.to_string(),
    })
}

fn load_font(path: &Path) -> Result<Font, AtlasError> {
    load_font_at(path, 0)
}

/// Which face produced a glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaceId {
    Primary(FontStyle),
    Fallback(usize),
}

/// One lazily loaded fallback face.
///
/// `unavailable` only records a file/parse failure, not "does not cover this
/// character": the same font may cover the next one.
struct FallbackFace {
    path: PathBuf,
    collection_index: u32,
    font: Option<Font>,
    unavailable: bool,
}

impl FallbackFace {
    fn new(path: PathBuf, collection_index: u32) -> Self {
        Self {
            path,
            collection_index,
            font: None,
            unavailable: false,
        }
    }

    /// Cheap coverage probe using `ttf-parser`'s cmap only, so a missing emoji
    /// does not make fontdue parse every 18 MB CJK collection in the chain.
    fn covers(&self, ch: char) -> bool {
        let Ok(bytes) = std::fs::read(&self.path) else {
            return false;
        };
        OwnedFace::from_vec(bytes, self.collection_index)
            .ok()
            .is_some_and(|face| face.as_face_ref().glyph_index(ch).is_some())
    }
}

/// Result of choosing the face for one character.
struct FaceSelection {
    face: FaceId,
    style_fallback: bool,
    glyph_fallback: bool,
    missing: bool,
    fonts_loaded: u64,
}

/// Regular + optional bold/italic faces sharing one atlas, plus the on-demand
/// CJK/symbol/emoji outline fallback chain.
struct FontFaces {
    regular: Font,
    bold: Option<Font>,
    italic: Option<Font>,
    bold_italic: Option<Font>,
    regular_path: PathBuf,
    fallbacks: Vec<FallbackFace>,
}

impl FontFaces {
    fn load_regular(path: &Path) -> Result<Self, AtlasError> {
        let regular = load_font(path)?;
        let bold = try_load_variant(path, FontStyle::Bold);
        let italic = try_load_variant(path, FontStyle::Italic);
        let bold_italic = try_load_variant(path, FontStyle::BoldItalic);
        Ok(Self::with_faces(
            regular,
            bold,
            italic,
            bold_italic,
            path.to_path_buf(),
            true,
        ))
    }

    fn load_explicit(
        regular_path: &Path,
        bold: Option<&Path>,
        italic: Option<&Path>,
        bold_italic: Option<&Path>,
    ) -> Result<Self, AtlasError> {
        let regular = load_font(regular_path)?;
        let load_optional = |path: Option<&Path>| {
            path.filter(|path| path.is_file())
                .and_then(|path| load_font(path).ok())
        };
        Ok(Self::with_faces(
            regular,
            load_optional(bold),
            load_optional(italic),
            load_optional(bold_italic),
            regular_path.to_path_buf(),
            true,
        ))
    }

    fn with_faces(
        regular: Font,
        bold: Option<Font>,
        italic: Option<Font>,
        bold_italic: Option<Font>,
        regular_path: PathBuf,
        with_fallbacks: bool,
    ) -> Self {
        let fallbacks = if with_fallbacks {
            fallback_candidates()
                .into_iter()
                .map(|(path, index)| FallbackFace::new(path, index))
                .collect()
        } else {
            Vec::new()
        };
        Self {
            regular,
            bold,
            italic,
            bold_italic,
            regular_path,
            fallbacks,
        }
    }

    fn face(&self, style: FontStyle) -> (&Font, bool) {
        match style {
            FontStyle::Regular => (&self.regular, false),
            FontStyle::Bold => self
                .bold
                .as_ref()
                .map_or((&self.regular, true), |font| (font, false)),
            FontStyle::Italic => self
                .italic
                .as_ref()
                .map_or((&self.regular, true), |font| (font, false)),
            FontStyle::BoldItalic => self
                .bold_italic
                .as_ref()
                .map_or((&self.regular, true), |font| (font, false)),
        }
    }

    fn style_available(&self, style: FontStyle) -> bool {
        match style {
            FontStyle::Regular => true,
            FontStyle::Bold => self.bold.is_some(),
            FontStyle::Italic => self.italic.is_some(),
            FontStyle::BoldItalic => self.bold_italic.is_some(),
        }
    }

    /// Chooses the primary style face or the first fallback font that covers
    /// `ch`. Fallback fonts are parsed with fontdue only after a cheap cmap
    /// probe says they cover the character.
    fn select(&mut self, ch: char, style: FontStyle) -> FaceSelection {
        let (primary, style_fallback) = self.face(style);
        if primary.has_glyph(ch) {
            return FaceSelection {
                face: FaceId::Primary(style),
                style_fallback,
                glyph_fallback: false,
                missing: false,
                fonts_loaded: 0,
            };
        }

        let mut fonts_loaded = 0;
        for index in 0..self.fallbacks.len() {
            let fallback = &mut self.fallbacks[index];
            if fallback.unavailable {
                continue;
            }
            if fallback.font.is_none() {
                if !fallback.covers(ch) {
                    continue;
                }
                match load_font_at(&fallback.path, fallback.collection_index) {
                    Ok(font) => {
                        fallback.font = Some(font);
                        fonts_loaded += 1;
                    }
                    Err(_) => {
                        fallback.unavailable = true;
                        continue;
                    }
                }
            }
            if fallback.font.as_ref().is_some_and(|font| font.has_glyph(ch)) {
                return FaceSelection {
                    face: FaceId::Fallback(index),
                    style_fallback,
                    glyph_fallback: true,
                    missing: false,
                    fonts_loaded,
                };
            }
        }

        FaceSelection {
            face: FaceId::Primary(style),
            style_fallback,
            glyph_fallback: false,
            missing: true,
            fonts_loaded,
        }
    }

    fn font(&self, face: FaceId) -> &Font {
        match face {
            FaceId::Primary(primary_style) => self.face(primary_style).0,
            FaceId::Fallback(index) => self.fallbacks[index]
                .font
                .as_ref()
                .expect("selected fallback face is loaded"),
        }
    }

    fn fallback_paths(&self) -> Vec<PathBuf> {
        self.fallbacks.iter().map(|face| face.path.clone()).collect()
    }
}

/// Fallback outline fonts shared with the CPU renderer's chain. Paths absent on
/// the host are skipped; the first file that covers a character is parsed.
///
/// `TERVO_FALLBACK_FONTS` may add `path[:path...]`, with an optional
/// `#face_index` for collections, matching the app's CPU fallback syntax.
fn fallback_candidates() -> Vec<(PathBuf, u32)> {
    let mut candidates: Vec<(PathBuf, u32)> = [
        ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
        ("/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf", 0),
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 7),
        ("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 1),
        ("/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf", 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf", 0),
        ("/usr/share/fonts/truetype/ancient-scripts/Symbola_hint.ttf", 0),
        ("/usr/share/fonts/truetype/noto/NotoEmoji-Regular.ttf", 0),
        ("/usr/share/fonts/truetype/nerd-fonts/SymbolsNerdFontMono-Regular.ttf", 0),
        ("/usr/share/fonts/TTF/SymbolsNerdFontMono-Regular.ttf", 0),
    ]
    .into_iter()
    .map(|(path, index)| (PathBuf::from(path), index))
    .collect();

    if let Some(value) = std::env::var_os("TERVO_FALLBACK_FONTS") {
        for entry in value.to_string_lossy().split(':') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (path, index) = match entry.rsplit_once('#') {
                Some((path, index)) => (path, index.parse().unwrap_or(0)),
                None => (entry, 0),
            };
            candidates.push((PathBuf::from(path), index));
        }
    }
    candidates
}

/// Candidate family suffixes for common system monospace fonts.
fn style_variant_candidates(regular: &Path, style: FontStyle) -> Vec<PathBuf> {
    if style == FontStyle::Regular {
        return vec![regular.to_path_buf()];
    }
    let env_key = match style {
        FontStyle::Bold => "TERVO_WGPU_FONT_BOLD",
        FontStyle::Italic => "TERVO_WGPU_FONT_ITALIC",
        FontStyle::BoldItalic => "TERVO_WGPU_FONT_BOLD_ITALIC",
        FontStyle::Regular => unreachable!(),
    };
    let mut candidates = Vec::new();
    if let Some(value) = std::env::var_os(env_key) {
        candidates.push(PathBuf::from(value));
    }

    let parent = regular.parent().unwrap_or_else(|| Path::new(""));
    let stem = regular
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("font");
    let suffix = match style {
        FontStyle::Bold => "Bold",
        FontStyle::Italic => "Italic",
        FontStyle::BoldItalic => "BoldItalic",
        FontStyle::Regular => unreachable!(),
    };
    let dejavu_suffix = match style {
        FontStyle::Bold => "Bold",
        FontStyle::Italic => "Oblique",
        FontStyle::BoldItalic => "BoldOblique",
        FontStyle::Regular => unreachable!(),
    };

    // The regular file name either already contains "Regular" or is a bare
    // family name (DejaVuSansMono.ttf).
    if let Some(prefix) = stem.strip_suffix("-Regular") {
        candidates.push(parent.join(format!("{prefix}-{suffix}.ttf")));
    }
    if let Some(prefix) = stem.strip_suffix("Regular") {
        candidates.push(parent.join(format!("{prefix}{suffix}.ttf")));
    }
    // Weight-named regular faces ("JetBrainsMono-Medium") do not contain
    // "Regular"; strip the weight so the sibling Bold/Italic files are found.
    for weight in [
        "-ExtraBold",
        "-SemiBold",
        "-ExtraLight",
        "-Medium",
        "-Light",
        "-Thin",
    ] {
        if let Some(prefix) = stem.strip_suffix(weight) {
            candidates.push(parent.join(format!("{prefix}-{suffix}.ttf")));
            candidates.push(parent.join(format!("{prefix}{suffix}.ttf")));
        }
    }
    candidates.push(parent.join(format!("{stem}-{suffix}.ttf")));
    candidates.push(parent.join(format!("{stem}-{dejavu_suffix}.ttf")));

    candidates
}

fn try_load_variant(regular: &Path, style: FontStyle) -> Option<Font> {
    for candidate in style_variant_candidates(regular, style) {
        if candidate == regular {
            continue;
        }
        if let Ok(font) = load_font(&candidate) {
            return Some(font);
        }
    }
    None
}

/// Default system font candidates. Assets are preferred when present; the
/// repository does not currently bundle a monospace font.
pub fn default_font_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = std::env::var_os("TERVO_WGPU_FONT") {
        candidates.push(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("TERVO_FONT_PATH") {
        candidates.push(PathBuf::from(value));
    }
    candidates.extend(
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "assets/fonts/DejaVuSansMono.ttf",
            "crates/app/assets/DejaVuSansMono.ttf",
            "/System/Library/Fonts/Menlo.ttc",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    candidates
}

/// Resolves the font used by the atlas: `TERVO_WGPU_FONT` / `TERVO_FONT_PATH`
/// first, then DejaVu/JetBrains/Liberation and repository asset candidates.
pub fn discover_font_path() -> Result<PathBuf, AtlasError> {
    let tried = default_font_candidates();
    for path in &tried {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    Err(AtlasError::FontNotFound { tried })
}

/// Shelf allocator used by the coverage atlas.
///
/// Kept independent from wgpu/fontdue so packing failures can be unit tested
/// without a GPU adapter.
#[derive(Debug)]
struct ShelfAllocator {
    width: u32,
    height: u32,
    padding: u32,
    pen_x: u32,
    pen_y: u32,
    row_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AllocError {
    TooLarge,
    Full,
}

impl ShelfAllocator {
    fn new(width: u32, height: u32, padding: u32) -> Self {
        Self {
            width,
            height,
            padding,
            pen_x: 0,
            pen_y: 0,
            row_height: 0,
        }
    }

    fn allocate(&mut self, w: u32, h: u32) -> Result<(u32, u32), AllocError> {
        if w == 0 || h == 0 {
            return Ok((0, 0));
        }
        if w > self.width || h > self.height {
            return Err(AllocError::TooLarge);
        }
        if self.pen_x + w > self.width {
            self.pen_x = 0;
            self.pen_y = self.pen_y.saturating_add(self.row_height + self.padding);
            self.row_height = 0;
        }
        if self.pen_y + h > self.height {
            return Err(AllocError::Full);
        }
        let position = (self.pen_x, self.pen_y);
        self.pen_x = self.pen_x.saturating_add(w + self.padding);
        self.row_height = self.row_height.max(h);
        Ok(position)
    }
}

#[derive(Clone, Copy, Debug)]
struct CachedGlyph {
    slot: Option<GlyphSlot>,
    style_fallback: bool,
    glyph_fallback: bool,
    missing: bool,
}

/// GPU coverage texture plus its CPU mirror and glyph directory.
///
/// The CPU mirror exists so the validation-only reference compositor can read
/// the exact same coverage bytes the GPU sampled. The production render path
/// never downloads a framebuffer.
pub struct GlyphAtlas {
    width: u32,
    height: u32,
    padding: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    queue: wgpu::Queue,
    faces: FontFaces,
    allocator: ShelfAllocator,
    cpu_pixels: Vec<u8>,
    glyphs: HashMap<(char, u16, FontStyle), CachedGlyph>,
    stats: AtlasStats,
}

impl fmt::Debug for GlyphAtlas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlyphAtlas")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("padding", &self.padding)
            .field("cached_glyphs", &self.glyphs.len())
            .field("gpu_bytes", &self.gpu_bytes())
            .field("font_path", &self.faces.regular_path)
            .finish_non_exhaustive()
    }
}

impl GlyphAtlas {
    /// Creates an atlas from the discovered system font.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let path = discover_font_path()?;
        Self::from_path(device, queue, &path, config)
    }

    /// Creates an atlas from explicit regular/bold/italic/bold-italic files.
    ///
    /// Used by the app integration so the GPU path rasterizes the exact same
    /// font bytes as the CPU renderer. Missing variant files fall back to the
    /// regular face (counted as `style_fallbacks`).
    pub fn from_paths(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        regular: impl AsRef<Path>,
        bold: Option<&Path>,
        italic: Option<&Path>,
        bold_italic: Option<&Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let faces = FontFaces::load_explicit(regular.as_ref(), bold, italic, bold_italic)?;
        Self::from_faces(device, queue, faces, config)
    }

    /// Creates an atlas from an explicit regular font file.
    ///
    /// Bold/italic siblings are auto-discovered. The CJK/symbol/emoji fallback
    /// chain is loaded on demand.
    pub fn from_path(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        path: impl AsRef<Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let path = path.as_ref();
        let faces = FontFaces::load_regular(path)?;
        Self::from_faces(device, queue, faces, config)
    }

    /// Creates an atlas from in-memory font bytes (used by deterministic tests).
    ///
    /// No system fallback chain is attached so tests stay host-independent.
    pub fn from_font_bytes(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: Vec<u8>,
        source: impl AsRef<Path>,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let source = source.as_ref();
        let regular = Font::from_bytes(bytes, FontSettings::default()).map_err(|message| {
            AtlasError::FontParse {
                path: source.to_path_buf(),
                message: message.to_string(),
            }
        })?;
        let faces = FontFaces::with_faces(
            regular,
            None,
            None,
            None,
            source.to_path_buf(),
            false,
        );
        Self::from_faces(device, queue, faces, config)
    }

    fn from_faces(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        faces: FontFaces,
        config: AtlasConfig,
    ) -> Result<Self, AtlasError> {
        let max = device.limits().max_texture_dimension_2d;
        if config.width == 0
            || config.height == 0
            || config.width > max
            || config.height > max
            || config.padding >= config.width
            || config.padding >= config.height
        {
            return Err(AtlasError::InvalidConfig {
                width: config.width,
                height: config.height,
                max,
            });
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tervo-wgpu-glyph-atlas"),
            size: wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ATLAS_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Create a defined zero-filled atlas. Only glyph rectangles are ever
        // sampled, but zero padding keeps debugging readbacks deterministic.
        let zeros = vec![0u8; (config.width as usize) * (config.height as usize)];
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &zeros,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(config.width),
                rows_per_image: Some(config.height),
            },
            wgpu::Extent3d {
                width: config.width,
                height: config.height,
                depth_or_array_layers: 1,
            },
        );

        Ok(Self {
            width: config.width,
            height: config.height,
            padding: config.padding,
            texture,
            view,
            queue: queue.clone(),
            faces,
            allocator: ShelfAllocator::new(config.width, config.height, config.padding),
            cpu_pixels: zeros,
            glyphs: HashMap::new(),
            stats: AtlasStats::default(),
        })
    }

    /// Atlas width in texels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Atlas height in texels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The GPU texture view bound by the text pipeline.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The underlying `R8Unorm` texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// GPU memory occupied by the coverage texture, in bytes.
    pub fn gpu_bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Number of distinct `(glyph, size, style)` entries, including
    /// whitespace/empty outlines.
    pub fn cached_glyph_count(&self) -> usize {
        self.glyphs.len()
    }

    /// Font file backing the regular face.
    pub fn font_path(&self) -> &Path {
        &self.faces.regular_path
    }

    /// Configured fallback font paths (diagnostics/evidence; not all are
    /// parsed, only faces that cover a missing character are loaded).
    pub fn fallback_font_paths(&self) -> Vec<PathBuf> {
        self.faces.fallback_paths()
    }

    /// Whether a real bold/italic face was loaded.
    pub fn style_available(&self, style: FontStyle) -> bool {
        self.faces.style_available(style)
    }

    /// CPU mirror of the coverage atlas. Validation/reference only.
    pub fn cpu_pixels(&self) -> &[u8] {
        &self.cpu_pixels
    }

    /// Cumulative counters. Take a snapshot before and after a frame to get
    /// per-frame deltas.
    pub fn stats(&self) -> AtlasStats {
        self.stats
    }

    /// Monospace cell metrics for a pixel size, derived from the regular face.
    pub fn metrics(&self, px: u16) -> FontMetrics {
        let px_f = f32::from(px.max(1));
        let advance = self.faces.regular.metrics('M', px_f).advance_width;
        let line = self.faces.regular.horizontal_line_metrics(px_f);
        let (ascent, descent, new_line_size) = line.map_or((px_f * 0.8, -px_f * 0.2, px_f), |m| {
            (m.ascent, m.descent, m.new_line_size)
        });
        FontMetrics {
            cell_width: advance.round().max(1.0),
            // +2 leaves one pixel above the ascent and below the descent.
            cell_height: new_line_size.round().max(1.0) + 2.0,
            baseline: ascent.round() + 1.0,
            ascent,
            descent,
        }
    }

    /// Returns an atlas slot for `ch` at `px`, rasterizing and uploading on a
    /// miss. Empty glyphs (space) are cached as `None`.
    ///
    /// On [`AtlasError::Full`]/[`AtlasError::GlyphTooLarge`] the cache and
    /// counter state reflect real work, but no render pass is submitted by the
    /// caller, so the target texture is left untouched.
    pub fn ensure(
        &mut self,
        ch: char,
        px: u16,
        style: FontStyle,
    ) -> Result<GlyphLookup, AtlasError> {
        let px = px.max(1);
        self.stats.lookups += 1;
        let key = (ch, px, style);
        if let Some(cached) = self.glyphs.get(&key) {
            self.stats.hits += 1;
            if cached.style_fallback {
                self.stats.style_fallbacks += 1;
            }
            if cached.glyph_fallback {
                self.stats.glyph_fallbacks += 1;
            }
            if cached.missing {
                self.stats.missing_glyphs += 1;
            }
            return Ok(GlyphLookup {
                slot: cached.slot,
                style_fallback: cached.style_fallback,
                glyph_fallback: cached.glyph_fallback,
                missing: cached.missing,
            });
        }

        self.stats.misses += 1;
        let selection = self.faces.select(ch, style);
        if selection.style_fallback {
            self.stats.style_fallbacks += 1;
        }
        self.stats.fallback_fonts_loaded += selection.fonts_loaded;
        if selection.glyph_fallback {
            self.stats.glyph_fallbacks += 1;
        }

        // A character no face covers must never be rasterized from the wrong
        // font (fontdue would produce .notdef / tofu). Report it so the text
        // layer can draw an explicit '?' marker and count it.
        if selection.missing {
            self.stats.missing_glyphs += 1;
            self.glyphs.insert(
                key,
                CachedGlyph {
                    slot: None,
                    style_fallback: selection.style_fallback,
                    glyph_fallback: false,
                    missing: true,
                },
            );
            return Ok(GlyphLookup {
                slot: None,
                style_fallback: selection.style_fallback,
                glyph_fallback: false,
                missing: true,
            });
        }

        let font = self.faces.font(selection.face);
        let (metrics, bitmap) = font.rasterize(ch, f32::from(px));
        self.stats.rasterized += 1;

        if metrics.width == 0 || metrics.height == 0 {
            self.stats.empty_glyphs += 1;
            self.glyphs.insert(
                key,
                CachedGlyph {
                    slot: None,
                    style_fallback: selection.style_fallback,
                    glyph_fallback: selection.glyph_fallback,
                    missing: false,
                },
            );
            return Ok(GlyphLookup {
                slot: None,
                style_fallback: selection.style_fallback,
                glyph_fallback: selection.glyph_fallback,
                missing: false,
            });
        }

        let w = metrics.width as u32;
        let h = metrics.height as u32;
        let (x, y) = match self.allocator.allocate(w, h) {
            Ok(position) => position,
            Err(AllocError::TooLarge) => {
                self.stats.full_errors += 1;
                return Err(AtlasError::GlyphTooLarge {
                    ch,
                    style,
                    px,
                    width: w,
                    height: h,
                    atlas_width: self.width,
                    atlas_height: self.height,
                });
            }
            Err(AllocError::Full) => {
                self.stats.full_errors += 1;
                return Err(AtlasError::Full {
                    ch,
                    style,
                    px,
                    width: w,
                    height: h,
                    atlas_width: self.width,
                    atlas_height: self.height,
                });
            }
        };

        // Upload only the glyph rectangle; never the atlas or a framebuffer.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &bitmap,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.stats.uploads += 1;
        self.stats.uploaded_bytes += u64::from(w) * u64::from(h);

        // CPU mirror for the reference compositor.
        let atlas_width = self.width as usize;
        for row in 0..h as usize {
            let src = &bitmap[row * w as usize..(row + 1) * w as usize];
            let dst = (y as usize + row) * atlas_width + x as usize;
            self.cpu_pixels[dst..dst + w as usize].copy_from_slice(src);
        }

        let slot = GlyphSlot {
            x,
            y,
            w,
            h,
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            advance: metrics.advance_width.round() as i32,
        };
        self.glyphs.insert(
            key,
            CachedGlyph {
                slot: Some(slot),
                style_fallback: selection.style_fallback,
                glyph_fallback: selection.glyph_fallback,
                missing: false,
            },
        );
        Ok(GlyphLookup {
            slot: Some(slot),
            style_fallback: selection.style_fallback,
            glyph_fallback: selection.glyph_fallback,
            missing: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelf_allocator_packs_rows_and_reports_full() {
        let mut allocator = ShelfAllocator::new(16, 8, 1);
        assert_eq!(allocator.allocate(6, 3), Ok((0, 0)));
        assert_eq!(allocator.allocate(6, 3), Ok((7, 0)));
        // Third 6px glyph wraps to the next shelf.
        assert_eq!(allocator.allocate(6, 3), Ok((0, 4)));
        // 3px high glyph still fits on the second shelf.
        assert_eq!(allocator.allocate(6, 3), Ok((7, 4)));
        // A third shelf would start at y=8, which is outside the atlas.
        assert_eq!(allocator.allocate(6, 1), Err(AllocError::Full));
    }

    #[test]
    fn shelf_allocator_rejects_glyphs_larger_than_the_atlas() {
        let mut allocator = ShelfAllocator::new(16, 8, 1);
        assert_eq!(allocator.allocate(17, 1), Err(AllocError::TooLarge));
        assert_eq!(allocator.allocate(1, 9), Err(AllocError::TooLarge));
    }

    #[test]
    fn empty_bitmap_allocates_nothing() {
        let mut allocator = ShelfAllocator::new(16, 8, 1);
        assert_eq!(allocator.allocate(0, 0), Ok((0, 0)));
        assert_eq!(allocator.pen_x, 0);
        assert_eq!(allocator.pen_y, 0);
    }

    #[test]
    fn config_is_part_of_the_public_contract() {
        let config = AtlasConfig::default();
        assert_eq!(config.width, DEFAULT_ATLAS_SIZE);
        assert_eq!(config.padding, DEFAULT_PADDING);
        assert_eq!(ATLAS_FORMAT, wgpu::TextureFormat::R8Unorm);
        assert_eq!(config.width as u64 * config.height as u64, 1024 * 1024);
    }

    #[test]
    fn style_resolution_covers_all_attribute_combinations() {
        assert_eq!(
            FontStyle::from_attributes(TextAttributes::default()),
            FontStyle::Regular
        );
        assert_eq!(
            FontStyle::from_attributes(TextAttributes {
                bold: true,
                ..TextAttributes::default()
            }),
            FontStyle::Bold
        );
        assert_eq!(
            FontStyle::from_attributes(TextAttributes {
                italic: true,
                ..TextAttributes::default()
            }),
            FontStyle::Italic
        );
        assert_eq!(
            FontStyle::from_attributes(TextAttributes {
                bold: true,
                italic: true,
                ..TextAttributes::default()
            }),
            FontStyle::BoldItalic
        );
        // Faint/dim maps to the bold face, matching the CPU renderer.
        assert_eq!(
            FontStyle::from_attributes(TextAttributes {
                dim: true,
                ..TextAttributes::default()
            }),
            FontStyle::Bold
        );
        assert_eq!(
            FontStyle::from_attributes(TextAttributes {
                dim: true,
                italic: true,
                ..TextAttributes::default()
            }),
            FontStyle::BoldItalic
        );
    }
}
