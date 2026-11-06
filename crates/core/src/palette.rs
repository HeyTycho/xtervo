// SPDX-License-Identifier: GPL-3.0-only
//! 颜色解析：把 alacritty 的 `Color` 解析成 0x00RRGGBB。
//!
//! 索引表完全按 alacritty 的 `Colors`（269 项）语义排布：
//! 0..16 主题色、16..232 色立方、232..256 灰阶、256 前景、257 背景、258 光标、259.. 暗色。

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

pub const COUNT: usize = 269;

/// 内置配色方案。终端 16 色、前景/背景/光标，以及界面 chrome 颜色都由它决定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    TokyoNight,
    CatppuccinMocha,
    CatppuccinMacchiato,
    CatppuccinFrappe,
    CatppuccinLatte,
    Dracula,
    Nord,
    GruvboxDark,
    OneDark,
    SolarizedLight,
    /// Ubuntu/GNOME 系统深色（中性灰 + Yaru 橙）
    YaruDark,
    /// Ubuntu/GNOME 系统浅色
    YaruLight,
}

impl Theme {
    /// 设置面板里的展示顺序。
    pub const ALL: [Theme; 12] = [
        Theme::YaruDark,
        Theme::YaruLight,
        Theme::TokyoNight,
        Theme::CatppuccinMocha,
        Theme::CatppuccinMacchiato,
        Theme::CatppuccinFrappe,
        Theme::CatppuccinLatte,
        Theme::Dracula,
        Theme::Nord,
        Theme::GruvboxDark,
        Theme::OneDark,
        Theme::SolarizedLight,
    ];

    /// 容错解析：支持缩写、旧配置名（mocha / latte）和 `_`/空格分隔。
    pub fn from_name(name: &str) -> Self {
        let key = name.trim().to_ascii_lowercase().replace([' ', '_'], "-");
        match key.as_str() {
            "tokyo-night" | "tokyonight" | "tokyo" => Theme::TokyoNight,
            "catppuccin-mocha" | "mocha" => Theme::CatppuccinMocha,
            "catppuccin-macchiato" | "macchiato" => Theme::CatppuccinMacchiato,
            "catppuccin-frappe" | "frappe" | "frappé" => Theme::CatppuccinFrappe,
            "catppuccin-latte" | "latte" => Theme::CatppuccinLatte,
            "dracula" => Theme::Dracula,
            "nord" => Theme::Nord,
            "gruvbox-dark" | "gruvbox" => Theme::GruvboxDark,
            "one-dark" | "onedark" => Theme::OneDark,
            "solarized-light" | "solarized" => Theme::SolarizedLight,
            "yaru-dark" | "yaru" | "ubuntu-dark" | "system-dark" => Theme::YaruDark,
            "yaru-light" | "ubuntu-light" | "system-light" => Theme::YaruLight,
            _ => Theme::default(),
        }
    }

    /// 写进配置文件的稳定 id。
    pub fn id(self) -> &'static str {
        match self {
            Theme::TokyoNight => "tokyo-night",
            Theme::CatppuccinMocha => "catppuccin-mocha",
            Theme::CatppuccinMacchiato => "catppuccin-macchiato",
            Theme::CatppuccinFrappe => "catppuccin-frappe",
            Theme::CatppuccinLatte => "catppuccin-latte",
            Theme::Dracula => "dracula",
            Theme::Nord => "nord",
            Theme::GruvboxDark => "gruvbox-dark",
            Theme::OneDark => "one-dark",
            Theme::SolarizedLight => "solarized-light",
            Theme::YaruDark => "yaru-dark",
            Theme::YaruLight => "yaru-light",
        }
    }

    /// 设置面板里显示的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Theme::TokyoNight => "Tokyo Night",
            Theme::CatppuccinMocha => "Mocha",
            Theme::CatppuccinMacchiato => "Macchiato",
            Theme::CatppuccinFrappe => "Frappe",
            Theme::CatppuccinLatte => "Latte",
            Theme::Dracula => "Dracula",
            Theme::Nord => "Nord",
            Theme::GruvboxDark => "Gruvbox",
            Theme::OneDark => "One Dark",
            Theme::SolarizedLight => "Solarized",
            Theme::YaruDark => "Yaru Dark",
            Theme::YaruLight => "Yaru Light",
        }
    }

    pub fn fg(self) -> u32 {
        self.spec().fg
    }

    pub fn bg(self) -> u32 {
        self.spec().bg
    }

    pub fn cursor(self) -> u32 {
        self.spec().cursor
    }

    /// 该主题是深色还是浅色（auto 配对、设置面板分组用）。
    pub fn is_dark(self) -> bool {
        !matches!(self, Theme::CatppuccinLatte | Theme::SolarizedLight | Theme::YaruLight)
    }

    /// 界面（标签栏、菜单、设置面板、搜索框）配色。
    pub fn ui(self) -> UiColors {
        match self {
            Theme::TokyoNight => UiColors {
                bg: 0x001a1b26,
                surface: 0x0016161e,
                raise: 0x0024283b,
                hover: 0x00292e42,
                border: 0x003b4261,
                text: 0x00c0caf5,
                subtext: 0x00a9b1d6,
                muted: 0x00565f89,
                accent: 0x007aa2f7,
                accent_text: 0x001a1b26,
                danger: 0x00f7768e,
            },
            Theme::CatppuccinMocha => UiColors {
                bg: 0x001e1e2e,
                surface: 0x00181825,
                raise: 0x00313244,
                hover: 0x0045475a,
                border: 0x00585b70,
                text: 0x00cdd6f4,
                subtext: 0x00a6adc8,
                muted: 0x006c7086,
                accent: 0x0089b4fa,
                accent_text: 0x0011111b,
                danger: 0x00f38ba8,
            },
            Theme::CatppuccinMacchiato => UiColors {
                bg: 0x0024273a,
                surface: 0x001e2030,
                raise: 0x00363a4f,
                hover: 0x00494d64,
                border: 0x005b6078,
                text: 0x00cad3f5,
                subtext: 0x00b8c0e0,
                muted: 0x008087a2,
                accent: 0x008aadf4,
                accent_text: 0x00181926,
                danger: 0x00ed8796,
            },
            Theme::CatppuccinFrappe => UiColors {
                bg: 0x00303446,
                surface: 0x00292c3c,
                raise: 0x00414559,
                hover: 0x0051576d,
                border: 0x00626880,
                text: 0x00c6d0f5,
                subtext: 0x00b5bfe2,
                muted: 0x00838ba7,
                accent: 0x008caaee,
                accent_text: 0x00232634,
                danger: 0x00e78284,
            },
            Theme::CatppuccinLatte => UiColors {
                bg: 0x00eff1f5,
                surface: 0x00e6e9ef,
                raise: 0x00dce0e8,
                hover: 0x00ccd0da,
                border: 0x00bcc0cc,
                text: 0x004c4f69,
                subtext: 0x005c5f77,
                muted: 0x008c8fa1,
                accent: 0x001e66f5,
                accent_text: 0x00eff1f5,
                danger: 0x00d20f39,
            },
            Theme::Dracula => UiColors {
                bg: 0x00282a36,
                surface: 0x0021222c,
                raise: 0x00343746,
                hover: 0x0044475a,
                border: 0x0055586b,
                text: 0x00f8f8f2,
                subtext: 0x00e2e2dc,
                muted: 0x006272a4,
                accent: 0x00bd93f9,
                accent_text: 0x0021222c,
                danger: 0x00ff5555,
            },
            Theme::Nord => UiColors {
                bg: 0x002e3440,
                surface: 0x00272c36,
                raise: 0x003b4252,
                hover: 0x00434c5e,
                border: 0x004c566a,
                text: 0x00eceff4,
                subtext: 0x00d8dee9,
                muted: 0x007b88a1,
                accent: 0x0088c0d0,
                accent_text: 0x002e3440,
                danger: 0x00bf616a,
            },
            Theme::GruvboxDark => UiColors {
                bg: 0x00282828,
                surface: 0x001d2021,
                raise: 0x003c3836,
                hover: 0x00504945,
                border: 0x00665c54,
                text: 0x00ebdbb2,
                subtext: 0x00d5c4a1,
                muted: 0x00928374,
                accent: 0x00fabd2f,
                accent_text: 0x00282828,
                danger: 0x00fb4934,
            },
            Theme::OneDark => UiColors {
                bg: 0x00282c34,
                surface: 0x0021252b,
                raise: 0x00323842,
                hover: 0x003e4451,
                border: 0x004b5263,
                text: 0x00abb2bf,
                subtext: 0x009da5b4,
                muted: 0x005c6370,
                accent: 0x0061afef,
                accent_text: 0x00282c34,
                danger: 0x00e06c75,
            },
            Theme::SolarizedLight => UiColors {
                bg: 0x00fdf6e3,
                surface: 0x00eee8d5,
                raise: 0x00e5ddc7,
                hover: 0x00d9d2c2,
                border: 0x00ccc4ae,
                text: 0x00657b83,
                subtext: 0x00586e75,
                muted: 0x0093a1a1,
                accent: 0x00268bd2,
                accent_text: 0x00fdf6e3,
                danger: 0x00dc322f,
            },
            Theme::YaruDark => UiColors {
                bg: 0x001e1e1e,
                surface: 0x00242424,
                raise: 0x00363636,
                hover: 0x00464646,
                border: 0x005c5c5c,
                text: 0x00f6f5f4,
                subtext: 0x00cfcbc6,
                muted: 0x008e8a85,
                accent: 0x00e95420,
                accent_text: 0x00ffffff,
                danger: 0x00e01b24,
            },
            Theme::YaruLight => UiColors {
                bg: 0x00fafafa,
                surface: 0x00f2f2f2,
                raise: 0x00e6e6e6,
                hover: 0x00d9d9d9,
                border: 0x00c9c9c9,
                text: 0x002d2d2d,
                subtext: 0x005e5e5e,
                muted: 0x008a8a8a,
                accent: 0x00e95420,
                accent_text: 0x00ffffff,
                danger: 0x00c01c28,
            },
        }
    }

    pub fn spec(self) -> PaletteSpec {
        match self {
            Theme::TokyoNight => PaletteSpec {
                fg: 0x00c0caf5,
                bg: 0x001a1b26,
                cursor: 0x00c0caf5,
                bright_fg: 0x00c0caf5,
                dim_fg: 0x00a9b1d6,
                ansi16: [
                    0x0015161e, 0x00f7768e, 0x009ece6a, 0x00e0af68,
                    0x007aa2f7, 0x00bb9af7, 0x007dcfff, 0x00a9b1d6,
                    0x00414868, 0x00f7768e, 0x009ece6a, 0x00e0af68,
                    0x007aa2f7, 0x00bb9af7, 0x007dcfff, 0x00c0caf5,
                ],
            },
            Theme::CatppuccinMocha => PaletteSpec {
                fg: 0x00cdd6f4,
                bg: 0x001e1e2e,
                cursor: 0x00f5e0dc,
                bright_fg: 0x00a6adc8,
                dim_fg: 0x009aa0b8,
                ansi16: [
                    0x0045475a, 0x00f38ba8, 0x00a6e3a1, 0x00f9e2af,
                    0x0089b4fa, 0x00f5c2e7, 0x0094e2d5, 0x00bac2de,
                    0x00585b70, 0x00f38ba8, 0x00a6e3a1, 0x00f9e2af,
                    0x0089b4fa, 0x00f5c2e7, 0x0094e2d5, 0x00a6adc8,
                ],
            },
            Theme::CatppuccinMacchiato => PaletteSpec {
                fg: 0x00cad3f5,
                bg: 0x0024273a,
                cursor: 0x00f4dbd6,
                bright_fg: 0x00b8c0e0,
                dim_fg: 0x00a5adcb,
                ansi16: [
                    0x00494d64, 0x00ed8796, 0x00a6da95, 0x00eed49f,
                    0x008aadf4, 0x00c6a0f6, 0x008bd5ca, 0x00b8c0e0,
                    0x005b6078, 0x00ed8796, 0x00a6da95, 0x00eed49f,
                    0x008aadf4, 0x00c6a0f6, 0x008bd5ca, 0x00cad3f5,
                ],
            },
            Theme::CatppuccinFrappe => PaletteSpec {
                fg: 0x00c6d0f5,
                bg: 0x00303446,
                cursor: 0x00f2d5cf,
                bright_fg: 0x00b5bfe2,
                dim_fg: 0x00a5adce,
                ansi16: [
                    0x0051576d, 0x00e78284, 0x00a6d189, 0x00e5c890,
                    0x008caaee, 0x00ca9ee6, 0x0081c8be, 0x00b5bfe2,
                    0x00626880, 0x00e78284, 0x00a6d189, 0x00e5c890,
                    0x008caaee, 0x00ca9ee6, 0x0081c8be, 0x00c6d0f5,
                ],
            },
            Theme::CatppuccinLatte => PaletteSpec {
                fg: 0x004c4f69,
                bg: 0x00eff1f5,
                cursor: 0x00dc8a78,
                bright_fg: 0x005c5f77,
                dim_fg: 0x008c8fa1,
                ansi16: [
                    0x005c5f77, 0x00d20f39, 0x0040a02b, 0x00df8e1d,
                    0x001e66f5, 0x00ea76cb, 0x00179299, 0x009ca0b0,
                    0x006c6f85, 0x00d20f39, 0x0040a02b, 0x00df8e1d,
                    0x001e66f5, 0x00ea76cb, 0x00179299, 0x00bcc0cc,
                ],
            },
            Theme::Dracula => PaletteSpec {
                fg: 0x00f8f8f2,
                bg: 0x00282a36,
                cursor: 0x00f8f8f2,
                bright_fg: 0x00ffffff,
                dim_fg: 0x006272a4,
                ansi16: [
                    0x0021222c, 0x00ff5555, 0x0050fa7b, 0x00f1fa8c,
                    0x00bd93f9, 0x00ff79c6, 0x008be9fd, 0x00f8f8f2,
                    0x006272a4, 0x00ff6e6e, 0x0069ff94, 0x00ffffa5,
                    0x00d6acff, 0x00ff92df, 0x00a4ffff, 0x00ffffff,
                ],
            },
            Theme::Nord => PaletteSpec {
                fg: 0x00d8dee9,
                bg: 0x002e3440,
                cursor: 0x00d8dee9,
                bright_fg: 0x00eceff4,
                dim_fg: 0x00a3abbd,
                ansi16: [
                    0x003b4252, 0x00bf616a, 0x00a3be8c, 0x00ebcb8b,
                    0x0081a1c1, 0x00b48ead, 0x0088c0d0, 0x00e5e9f0,
                    0x004c566a, 0x00bf616a, 0x00a3be8c, 0x00ebcb8b,
                    0x0081a1c1, 0x00b48ead, 0x008fbcbb, 0x00eceff4,
                ],
            },
            Theme::GruvboxDark => PaletteSpec {
                fg: 0x00ebdbb2,
                bg: 0x00282828,
                cursor: 0x00ebdbb2,
                bright_fg: 0x00fbf1c7,
                dim_fg: 0x00bdae93,
                ansi16: [
                    0x00282828, 0x00cc241d, 0x0098971a, 0x00d79921,
                    0x00458588, 0x00b16286, 0x00689d6a, 0x00a89984,
                    0x00928374, 0x00fb4934, 0x00b8bb26, 0x00fabd2f,
                    0x0083a598, 0x00d3869b, 0x008ec07c, 0x00ebdbb2,
                ],
            },
            Theme::OneDark => PaletteSpec {
                fg: 0x00abb2bf,
                bg: 0x00282c34,
                cursor: 0x00528bff,
                bright_fg: 0x00ffffff,
                dim_fg: 0x007f848e,
                ansi16: [
                    0x00282c34, 0x00e06c75, 0x0098c379, 0x00e5c07b,
                    0x0061afef, 0x00c678dd, 0x0056b6c2, 0x00abb2bf,
                    0x005c6370, 0x00e06c75, 0x0098c379, 0x00e5c07b,
                    0x0061afef, 0x00c678dd, 0x0056b6c2, 0x00ffffff,
                ],
            },
            Theme::SolarizedLight => PaletteSpec {
                fg: 0x00657b83,
                bg: 0x00fdf6e3,
                cursor: 0x00586e75,
                bright_fg: 0x00586e75,
                dim_fg: 0x0093a1a1,
                ansi16: [
                    0x00073642, 0x00dc322f, 0x00859900, 0x00b58900,
                    0x00268bd2, 0x00d33682, 0x002aa198, 0x00eee8d5,
                    0x00002b36, 0x00cb4b16, 0x00586e75, 0x00657b83,
                    0x00839496, 0x006c71c4, 0x0093a1a1, 0x00fdf6e3,
                ],
            },
            Theme::YaruDark => PaletteSpec {
                fg: 0x00f6f5f4,
                bg: 0x001e1e1e,
                cursor: 0x00f6f5f4,
                bright_fg: 0x00ffffff,
                dim_fg: 0x00c8c5c0,
                ansi16: [
                    0x002e3436, 0x00cc0000, 0x004e9a06, 0x00c4a000,
                    0x003465a4, 0x0075507b, 0x0006989a, 0x00d3d7cf,
                    0x00555753, 0x00ef2929, 0x008ae234, 0x00fce94f,
                    0x00729fcf, 0x00ad7fa8, 0x0034e2e2, 0x00eeeeec,
                ],
            },
            Theme::YaruLight => PaletteSpec {
                fg: 0x002d2d2d,
                bg: 0x00fafafa,
                cursor: 0x00e95420,
                bright_fg: 0x00000000,
                dim_fg: 0x005e5e5e,
                ansi16: [
                    0x002e3436, 0x00cc0000, 0x004e9a06, 0x00c4a000,
                    0x003465a4, 0x0075507b, 0x0006989a, 0x00d3d7cf,
                    0x00555753, 0x00ef2929, 0x008ae234, 0x00fce94f,
                    0x00729fcf, 0x00ad7fa8, 0x0034e2e2, 0x00eeeeec,
                ],
            },
        }
    }
}

/// 界面 chrome 的配色（不含终端单元格）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiColors {
    pub bg: u32,
    pub surface: u32,
    pub raise: u32,
    pub hover: u32,
    pub border: u32,
    pub text: u32,
    pub subtext: u32,
    pub muted: u32,
    pub accent: u32,
    pub accent_text: u32,
    pub danger: u32,
}

impl UiColors {
    /// 从任意终端配色推导界面颜色：用于导入 iTerm2 / Windows Terminal /
    /// Alacritty / kitty / Xresources 等只带 16 色的现成主题。
    pub fn derive(spec: &PaletteSpec) -> Self {
        fn lum(c: u32) -> f32 {
            let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
            0.299 * ch(16) + 0.587 * ch(8) + 0.114 * ch(0)
        }
        fn mix(a: u32, b: u32, t: f32) -> u32 {
            let ch = |s: u32| {
                let x = ((a >> s) & 0xff) as f32;
                let y = ((b >> s) & 0xff) as f32;
                (x + (y - x) * t).round().clamp(0.0, 255.0) as u32
            };
            ch(16) << 16 | ch(8) << 8 | ch(0)
        }
        let bg = spec.bg;
        let fg = spec.fg;
        let dark = lum(bg) < 0.5;
        let shift_target = if dark { 0x00ff_ffff } else { 0x0000_0000 };
        let surface = mix(bg, shift_target, 0.06);
        let raise = mix(bg, shift_target, if dark { 0.12 } else { 0.09 });
        let hover = mix(bg, shift_target, if dark { 0.20 } else { 0.15 });
        let border = mix(bg, fg, 0.28);
        let subtext = mix(fg, bg, 0.72);
        let muted = mix(fg, bg, 0.45);

        // 蓝色优先，其次青色/品红；对比度太低就退到前景色
        let min_contrast = 0.16;
        let mut accent = spec.ansi16[4];
        if (lum(accent) - lum(bg)).abs() < min_contrast {
            accent = spec.ansi16[6];
            if (lum(accent) - lum(bg)).abs() < min_contrast {
                accent = spec.ansi16[5];
                if (lum(accent) - lum(bg)).abs() < min_contrast {
                    accent = fg;
                }
            }
        }
        let accent_text = if lum(accent) > 0.55 { 0x0011_1111 } else { 0x00ff_ffff };
        let mut danger = spec.ansi16[1];
        if (lum(danger) - lum(bg)).abs() < 0.10 {
            danger = spec.ansi16[9];
        }
        Self {
            bg,
            surface,
            raise,
            hover,
            border,
            text: fg,
            subtext,
            muted,
            accent,
            accent_text,
            danger,
        }
    }
}

/// 终端配色的最小数据集：16 色 + 前景/背景/光标 + 亮/暗前景。
/// 内置主题和用户自定义主题（`themes/*.toml`）最终都归一化成它。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteSpec {
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
    pub bright_fg: u32,
    pub dim_fg: u32,
    pub ansi16: [u32; 16],
}

/// 默认主题：Tokyo Night。
pub const THEME_FG: u32 = 0x00c0caf5;
pub const THEME_BG: u32 = 0x001a1b26;
pub const THEME_CURSOR: u32 = 0x00c0caf5;

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

#[inline]
fn pack(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn xterm_indexed(i: u8, ansi16: &[u32; 16]) -> u32 {
    match i {
        0..=15 => ansi16[i as usize],
        16..=231 => {
            let i = i as usize - 16;
            let r = CUBE_LEVELS[i / 36];
            let g = CUBE_LEVELS[(i / 6) % 6];
            let b = CUBE_LEVELS[i % 6];
            pack(r, g, b)
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            pack(v, v, v)
        }
    }
}

#[derive(Clone, Debug)]
pub struct Palette {
    table: [u32; COUNT],
    pub fg: u32,
    pub bg: u32,
    pub cursor: u32,
    /// 粗体使用亮色：ANSI 0–7 / Named Black..White → 对应 Bright 变体。
    /// 默认开（与 GNOME Terminal `bold-is-bright` 一致）。
    pub bold_is_bright: bool,
    /// 自定义粗体颜色；None = 使用普通前景色。
    pub bold_color: Option<u32>,
    /// true = 忽略 `bold_color`，粗体仍用普通前景色（GNOME
    /// `bold-color-same-as-fg`）。
    pub bold_color_same_as_fg: bool,
}

impl Default for Palette {
    fn default() -> Self {
        Self::for_theme(Theme::default())
    }
}

impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        Self::from_spec(&theme.spec())
    }

    /// 从任意配色数据构建（内置主题 / 自定义主题共用）。
    pub fn from_spec(c: &PaletteSpec) -> Self {
        let mut table = [0u32; COUNT];
        for (i, slot) in table.iter_mut().take(16).enumerate() {
            *slot = c.ansi16[i];
        }
        for (i, slot) in table.iter_mut().enumerate().take(256).skip(16) {
            *slot = xterm_indexed(i as u8, &c.ansi16);
        }
        table[NamedColor::Foreground as usize] = c.fg;
        table[NamedColor::Background as usize] = c.bg;
        table[NamedColor::Cursor as usize] = c.cursor;
        // 259..=266 为 8 个暗色变体（按 2/3 亮度近似）
        for i in 0..8usize {
            let base = c.ansi16[i];
            let dim = |shift: u32| (((base >> shift) & 0xff) * 2 / 3) & 0xff;
            table[259 + i] = pack(dim(16) as u8, dim(8) as u8, dim(0) as u8);
        }
        table[NamedColor::BrightForeground as usize] = c.bright_fg;
        table[NamedColor::DimForeground as usize] = c.dim_fg;
        Self {
            table,
            fg: c.fg,
            bg: c.bg,
            cursor: c.cursor,
            bold_is_bright: true,
            bold_color: None,
            bold_color_same_as_fg: true,
        }
    }

    /// 覆盖粗体渲染策略（来自用户配置）。返回 self，便于链式构造。
    #[must_use]
    pub fn with_bold(
        mut self,
        bold_is_bright: bool,
        bold_color: Option<u32>,
        bold_color_same_as_fg: bool,
    ) -> Self {
        self.bold_is_bright = bold_is_bright;
        self.bold_color = bold_color;
        self.bold_color_same_as_fg = bold_color_same_as_fg;
        self
    }

    /// 解析单元格前景色，按 `bold-is-bright` / `bold-color` 规则处理粗体。
    ///
    /// - `bold-color-same-as-fg = false` 且配置了 `bold-color` 时，**默认前景色**
    ///   的粗体文字改用该颜色（显式 ANSI 色不受影响，和 VTE 一致）。
    /// - `bold-is-bright = true` 时，ANSI 0–7 / Named Black..White 的粗体改用
    ///   8–15 亮色。默认前景色不映射：内置主题的 `bright_fg` 并不保证更亮，
    ///   强行替换会改变所有粗体正文的颜色。
    pub fn resolve_fg(&self, color: Color, bold: bool) -> u32 {
        if bold {
            if !self.bold_color_same_as_fg {
                if let (Some(bold_color), Color::Named(NamedColor::Foreground)) =
                    (self.bold_color, color)
                {
                    return bold_color;
                }
            }
            if self.bold_is_bright {
                match color {
                    // 只映射标准 8 色；默认前景色/Foreground 不在这里换色，
                    // 否则所有粗体正文会跟着主题的 bright_fg（不保证更亮）改变。
                    Color::Named(
                        named @ (NamedColor::Black
                        | NamedColor::Red
                        | NamedColor::Green
                        | NamedColor::Yellow
                        | NamedColor::Blue
                        | NamedColor::Magenta
                        | NamedColor::Cyan
                        | NamedColor::White),
                    ) => return self.table[named.to_bright() as usize],
                    Color::Indexed(i) if i < 8 => return self.table[i as usize + 8],
                    _ => {},
                }
            }
        }
        self.resolve(color)
    }

    #[inline]
    pub fn resolve(&self, color: Color) -> u32 {
        match color {
            Color::Named(named) => self.table[named as usize],
            Color::Indexed(i) => self.table[i as usize],
            Color::Spec(Rgb { r, g, b }) => pack(r, g, b),
        }
    }

    /// 反色：光标块、inverse 单元格用。
    #[inline]
    pub fn opposite(&self, rgb: u32) -> u32 {
        const LUM: f32 = 0.299 * 255.0 + 0.587 * 255.0 + 0.114 * 255.0;
        let r = (rgb >> 16 & 0xff) as f32;
        let g = (rgb >> 8 & 0xff) as f32;
        let b = (rgb & 0xff) as f32;
        if 0.299 * r + 0.587 * g + 0.114 * b > LUM * 0.55 {
            self.bg
        } else {
            self.fg
        }
    }

    /// 按 xterm 颜色序号取色：0–255 索引色，256 前景、257 背景、258 光标色。
    /// 用于 OSC 4/10/11/12 的查询应答。
    #[inline]
    pub fn at(&self, index: usize) -> Option<u32> {
        match index {
            0..=255 => Some(self.table[index]),
            256 => Some(self.fg),
            257 => Some(self.bg),
            258 => Some(self.cursor),
            _ => None,
        }
    }

    #[inline]
    pub fn table(&self) -> &[u32; COUNT] {
        &self.table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_256_grayscale() {
        let p = Palette::default();
        assert_eq!(p.resolve(Color::Indexed(232)), 0x00080808);
        assert_eq!(p.resolve(Color::Indexed(255)), 0x00eeeeee);
    }

    #[test]
    fn cube_matches_xterm() {
        let p = Palette::default();
        assert_eq!(p.resolve(Color::Indexed(16)), 0x00000000);
        assert_eq!(p.resolve(Color::Indexed(196)), 0x00ff0000);
        assert_eq!(p.resolve(Color::Indexed(46)), 0x0000ff00);
        assert_eq!(p.resolve(Color::Indexed(21)), 0x000000ff);
    }

    #[test]
    fn theme_ids_roundtrip() {
        for theme in Theme::ALL {
            assert_eq!(Theme::from_name(theme.id()), theme);
            assert!(!theme.label().is_empty());
        }
    }

    #[test]
    fn theme_aliases() {
        assert_eq!(Theme::from_name("mocha"), Theme::CatppuccinMocha);
        assert_eq!(Theme::from_name("Catppuccin_Latte"), Theme::CatppuccinLatte);
        assert_eq!(Theme::from_name("tokyo night"), Theme::TokyoNight);
        assert_eq!(Theme::from_name("unknown-theme"), Theme::default());
    }

    #[test]
    fn all_themes_have_distinct_bg() {
        let mut seen = Vec::new();
        for theme in Theme::ALL {
            let bg = theme.bg();
            assert!(!seen.contains(&bg), "duplicate bg for {theme:?}");
            seen.push(bg);
            // 前景/背景亮度差不能太低，否则整屏不可读
            let lum = |c: u32| {
                0.299 * ((c >> 16) & 0xff) as f32
                    + 0.587 * ((c >> 8) & 0xff) as f32
                    + 0.114 * (c & 0xff) as f32
            };
            assert!((lum(theme.fg()) - lum(bg)).abs() > 60.0, "low contrast in {theme:?}");
        }
    }

    #[test]
    fn legacy_theme_names_still_resolve() {
        assert_eq!(Palette::for_theme(Theme::from_name("mocha")).bg, 0x001e1e2e);
        assert_eq!(Palette::for_theme(Theme::from_name("latte")).bg, 0x00eff1f5);
    }

    #[test]
    fn bold_is_bright_maps_only_ansi_0_to_7() {
        let palette = Palette::for_theme(Theme::YaruDark);
        let normal = palette.resolve(Color::Indexed(1));
        let bright = palette.resolve(Color::Indexed(9));
        assert_ne!(normal, bright, "测试主题的普通红/亮红应不同");
        assert_eq!(
            palette.resolve_fg(Color::Indexed(1), true),
            bright,
            "bold-is-bright 应把 ANSI 1 映射到 9"
        );
        assert_eq!(
            palette.resolve_fg(Color::Indexed(1), false),
            normal,
            "非粗体保持普通色"
        );
        assert_eq!(
            palette.resolve_fg(Color::Indexed(9), true),
            bright,
            "已经是亮色时不再上移"
        );
        // 默认前景色不映射：内置主题的 bright_fg 不保证更亮。
        assert_eq!(
            palette.resolve_fg(Color::Named(NamedColor::Foreground), true),
            palette.fg
        );
        // 关闭 bold-is-bright 后完全按普通色解析。
        let plain = Palette::for_theme(Theme::YaruDark).with_bold(false, None, true);
        assert_eq!(plain.resolve_fg(Color::Indexed(1), true), normal);
    }

    #[test]
    fn bold_color_only_applies_to_default_foreground_when_enabled() {
        let bold = 0x0012_3456;
        let palette =
            Palette::for_theme(Theme::YaruDark).with_bold(true, Some(bold), false);
        assert_eq!(
            palette.resolve_fg(Color::Named(NamedColor::Foreground), true),
            bold,
            "bold-color-same-as-fg=false 时默认前景色粗体用自定义色"
        );
        // 显式 ANSI 色不受 bold_color 覆盖（仍走 bold-is-bright）。
        assert_eq!(
            palette.resolve_fg(Color::Indexed(1), true),
            palette.resolve(Color::Indexed(9))
        );
        // same-as-fg=true 时忽略自定义色。
        let same = Palette::for_theme(Theme::YaruDark).with_bold(true, Some(bold), true);
        assert_eq!(
            same.resolve_fg(Color::Named(NamedColor::Foreground), true),
            same.fg
        );
        // 非粗体永远不用 bold_color。
        assert_eq!(
            palette.resolve_fg(Color::Named(NamedColor::Foreground), false),
            palette.fg
        );
    }

    #[test]
    fn derived_ui_colors_are_sane() {
        for theme in Theme::ALL {
            let spec = theme.spec();
            let ui = UiColors::derive(&spec);
            assert_eq!(ui.bg, spec.bg);
            assert_eq!(ui.text, spec.fg);
            assert_ne!(ui.surface, ui.bg, "{theme:?} surface 未分层");
            assert_ne!(ui.raise, ui.surface, "{theme:?} raise 未分层");
            assert_ne!(ui.accent, ui.bg, "{theme:?} accent 和背景撞色");
            assert_ne!(ui.accent_text, ui.accent, "{theme:?} accent 文本不可读");
        }
    }
}
