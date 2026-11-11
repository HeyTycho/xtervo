// SPDX-License-Identifier: GPL-3.0-only
//! Sixel 内联图片：流式 DCS 过滤 + 解码为直通 RGBA。
//!
//! 设计要点：
//! - **旁路过滤**：`alacritty_terminal` 不认识 Sixel，若把 DCS 原样喂给它，BEL
//!   终止的 Sixel 会让解析器一直停在字符串状态，后续 shell 输出被吞。因此 actor
//!   先把 DCS 从字节流里摘掉，只把非图片字节交给 vte。
//! - **跨 chunk**：DCS 头、payload、ST（`ESC \`）都可以被 PTY 读缓冲切断；状态机
//!   跨 `feed` 调用保持。
//! - **多终止符**：除标准 ST/C1 ST 外，还接受 BEL（dsh / img2sixel 常见）与
//!   CAN/SUB 中止。
//! - **安全上限**：原始 DCS 25 MiB、单图 16 MP / 单边 16384 px；任何非法/超限
//!   数据只丢图，绝不 panic。
//!
//! 解码输出是**非预乘** RGBA（未覆盖的像素 alpha=0），由 Slint 统一预乘。

/// 单条 Sixel DCS 原始 payload 上限（25 MiB）。
pub const MAX_RAW_BYTES: usize = 25 * 1024 * 1024;
/// 单图像素上限（16 MP）。
pub const MAX_PIXELS: usize = 16 * 1024 * 1024;
/// 单图单边像素上限。
pub const MAX_DIMENSION: usize = 16_384;
/// Sixel 颜色寄存器上限（0..=255）。
const MAX_COLORS: usize = 256;
/// DCS 头缓冲上限：合法 Sixel 头只有参数和 `q`，超出即按普通 DCS 放行。
const MAX_HEADER_BYTES: usize = 256;

const ESC: u8 = 0x1b;

/// 解码后的一张 Sixel 图片（直通 RGBA8，宽高为像素）。
pub struct SixelImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for SixelImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SixelImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish()
    }
}

/// 过滤结果：交给 vte 的普通字节，或一张已解码图片，顺序与输入一致。
pub enum SixelEvent {
    /// 原样交给 vte / OscScanner 的字节（已剔除 Sixel DCS）。
    Pass(Vec<u8>),
    /// 一张解码成功的图片；锚点由 actor 在收到该事件时读取当前光标。
    Image(SixelImage),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    /// `ESC P` 之后、final byte 之前。
    DcsHeader,
    /// 正在收集 Sixel payload。
    Sixel,
    /// Sixel payload 中看到 ESC，等待 `\`。
    SixelEscape,
    /// 非 Sixel DCS：原样放行到 ST。
    NonSixelDcs,
    NonSixelDcsEscape,
    Csi,
    Osc,
    OscEscape,
    /// SOS / PM / APC：原样放行到 ST。
    String,
    StringEscape,
    /// payload 超限后仍要吞到终止符。
    SixelOverflow,
    SixelOverflowEscape,
}

/// 流式 Sixel DCS 过滤器。
#[derive(Default)]
pub struct SixelFilter {
    state: State,
    /// 尚未决定去留的字节（ESC / DCS 头）。普通字节不经过这里。
    pending: Vec<u8>,
    /// DCS 头里是否出现过中间字节（`$` 等）。有中间字节的 `...q` 不是 Sixel。
    dcs_intermediate: bool,
    payload: Vec<u8>,
    /// 当前 payload 已见到的原始字节数（payload 超限后不再保存，但计数继续）。
    raw_len: usize,
    /// 本 chunk 里是否吞掉/暂存了字节；决定返回 None（原样放行）还是 Some。
    swallowed: bool,
}

impl SixelFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 跨 chunk 暂存、尚未决定去留的字节数（`ESC`/DCS 头）。
    ///
    /// 仅供审计/测试：正常流满足
    /// `输入 == 放行 + 明确消费的图片序列 + held`，任何静默丢字节都会让
    /// 这个账本对不上。不参与生产逻辑。
    #[inline]
    pub fn held_bytes(&self) -> usize {
        self.pending.len()
    }

    /// 当前是否处于 Sixel payload 中（actor 可据此抑制无意义唤醒）。
    #[inline]
    pub fn in_sixel(&self) -> bool {
        matches!(
            self.state,
            State::Sixel
                | State::SixelEscape
                | State::SixelOverflow
                | State::SixelOverflowEscape
        )
    }

    /// 扫描一段 PTY 字节。
    ///
    /// - 返回 `None`：本段不含 Sixel/悬空 ESC，调用方应把**原始** `bytes` 交给 vte。
    /// - 返回 `Some(events)`：按顺序处理 events；`Pass` 字节交给 vte，`Image`
    ///   表示此刻光标处新增一张图片。注意 `Some(vec![])` 也是有效的（整段都是
    ///   Sixel payload，不能把原文交给 vte）。
    ///
    /// `decode = false`（Sixel 关闭）时仍会过滤 DCS，只是不做解码，避免关闭配置
    /// 后 BEL 终止的 Sixel 污染解析器。
    pub fn feed(&mut self, bytes: &[u8], decode: bool) -> Option<Vec<SixelEvent>> {
        if self.state == State::Ground && self.pending.is_empty() && !contains_dcs_start(bytes)
        {
            return None;
        }

        let mut out: Vec<SixelEvent> = Vec::new();
        self.swallowed = false;
        let mut pass: Vec<u8> = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            match self.state {
                State::Ground => match b {
                    ESC => {
                        self.pending.clear();
                        self.pending.push(ESC);
                        self.state = State::Escape;
                    },
                    0x90 => {
                        // C1 DCS
                        self.pending.clear();
                        self.pending.push(b);
                        self.dcs_intermediate = false;
                        self.state = State::DcsHeader;
                    },
                    0x9d => {
                        pass.push(b);
                        self.state = State::Osc;
                    },
                    0x9b => {
                        pass.push(b);
                        self.state = State::Csi;
                    },
                    0x98 | 0x9e | 0x9f => {
                        pass.push(b);
                        self.state = State::String;
                    },
                    _ => pass.push(b),
                },
                State::Escape => match b {
                    b'P' => {
                        self.pending.push(b);
                        self.dcs_intermediate = false;
                        self.state = State::DcsHeader;
                    },
                    b'[' => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::Csi;
                    },
                    b']' => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::Osc;
                    },
                    b'X' | b'^' | b'_' => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::String;
                    },
                    ESC => {
                        // `ESC ESC`：前一个 ESC 作为普通字节放行，新的 ESC 继续等。
                        self.flush_pending(&mut pass);
                        self.pending.push(ESC);
                        self.state = State::Escape;
                    },
                    _ => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::Ground;
                    },
                },
                State::DcsHeader => match b {
                    ESC => {
                        // DCS 被新的 ESC 中止：把已缓冲的头原样交还给 vte。
                        self.flush_pending(&mut pass);
                        self.pending.push(ESC);
                        self.state = State::Escape;
                    },
                    0x9c => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::Ground;
                    },
                    0x18 | 0x1a => {
                        self.flush_pending(&mut pass);
                        pass.push(b);
                        self.state = State::Ground;
                    },
                    0x20..=0x2f => {
                        self.pending.push(b);
                        self.dcs_intermediate = true;
                        self.cap_header(&mut pass);
                    },
                    0x30..=0x3f => {
                        self.pending.push(b);
                        self.cap_header(&mut pass);
                    },
                    0x40..=0x7e => {
                        self.pending.push(b);
                        if b == b'q' && !self.dcs_intermediate {
                            // 真 Sixel：整段 DCS 头丢弃，开始收集 payload。
                            self.pending.clear();
                            self.payload.clear();
                            self.raw_len = 0;
                            self.swallowed = true;
                            self.state = State::Sixel;
                        } else {
                            // DECRQSS 等普通 DCS：原样放行到 ST。
                            self.flush_pending(&mut pass);
                            self.state = State::NonSixelDcs;
                        }
                    },
                    _ => {
                        self.pending.push(b);
                        self.cap_header(&mut pass);
                    },
                },
                State::Sixel => match b {
                    ESC => self.state = State::SixelEscape,
                    0x9c | 0x07 => {
                        self.finish_sixel(decode, &mut out);
                    },
                    0x18 | 0x1a => {
                        // CAN/SUB 中止：丢图。
                        self.payload.clear();
                        self.swallowed = true;
                        self.state = State::Ground;
                    },
                    _ => {
                        self.swallowed = true;
                        self.raw_len = self.raw_len.saturating_add(1);
                        if self.raw_len > MAX_RAW_BYTES {
                            self.payload.clear();
                            self.payload.shrink_to_fit();
                            self.state = State::SixelOverflow;
                        } else {
                            self.push_payload(b);
                        }
                    },
                },
                State::SixelEscape => {
                    if b == b'\\' {
                        self.finish_sixel(decode, &mut out);
                    } else if b == ESC {
                        // 连续 ESC：仍等待 `\`。
                    } else {
                        // 畸形 ESC 序列：中止图片，把当前字节按 Ground 重新处理。
                        self.payload.clear();
                        self.swallowed = true;
                        self.state = State::Ground;
                        continue;
                    }
                },
                State::SixelOverflow => match b {
                    ESC => self.state = State::SixelOverflowEscape,
                    0x9c | 0x07 | 0x18 | 0x1a => {
                        self.swallowed = true;
                        self.state = State::Ground;
                    },
                    _ => self.swallowed = true,
                },
                State::SixelOverflowEscape => {
                    if b == b'\\' {
                        self.state = State::Ground;
                    } else if b == ESC {
                        // 继续等
                    } else {
                        self.state = State::SixelOverflow;
                    }
                },
                State::NonSixelDcs => {
                    pass.push(b);
                    match b {
                        ESC => self.state = State::NonSixelDcsEscape,
                        0x9c | 0x18 | 0x1a => self.state = State::Ground,
                        _ => {},
                    }
                },
                State::NonSixelDcsEscape => {
                    pass.push(b);
                    match b {
                        b'\\' => self.state = State::Ground,
                        ESC => {},
                        _ => self.state = State::NonSixelDcs,
                    }
                },
                State::Csi => {
                    pass.push(b);
                    match b {
                        ESC => self.state = State::Escape,
                        0x40..=0x7e => self.state = State::Ground,
                        0x18 | 0x1a => self.state = State::Ground,
                        _ => {},
                    }
                },
                State::Osc => {
                    pass.push(b);
                    match b {
                        0x07 | 0x9c => self.state = State::Ground,
                        ESC => self.state = State::OscEscape,
                        _ => {},
                    }
                },
                State::OscEscape => {
                    pass.push(b);
                    match b {
                        b'\\' => self.state = State::Ground,
                        ESC => {},
                        _ => self.state = State::Ground,
                    }
                },
                State::String => {
                    pass.push(b);
                    match b {
                        0x9c | 0x18 | 0x1a => self.state = State::Ground,
                        ESC => self.state = State::StringEscape,
                        _ => {},
                    }
                },
                State::StringEscape => {
                    pass.push(b);
                    match b {
                        b'\\' => self.state = State::Ground,
                        ESC => {},
                        _ => self.state = State::String,
                    }
                },
            }
            i += 1;
        }

        if !pass.is_empty() {
            out.push(SixelEvent::Pass(pass));
        }
        if self.swallowed || !out.is_empty() || !self.pending.is_empty() {
            Some(out)
        } else {
            None
        }
    }

    #[inline]
    fn flush_pending(&mut self, pass: &mut Vec<u8>) {
        pass.extend_from_slice(&self.pending);
        self.pending.clear();
    }

    fn cap_header(&mut self, pass: &mut Vec<u8>) {
        if self.pending.len() > MAX_HEADER_BYTES {
            // 病态长 DCS 头：不再尝试识别 Sixel，按普通 DCS 放行。
            self.flush_pending(pass);
            self.state = State::NonSixelDcs;
        }
    }

    fn push_payload(&mut self, b: u8) {
        self.payload.push(b);
    }

    fn finish_sixel(&mut self, decode: bool, out: &mut Vec<SixelEvent>) {
        self.swallowed = true;
        if decode && !self.payload.is_empty() {
            if let Some(image) = decode_sixel(&self.payload) {
                out.push(SixelEvent::Image(image));
            }
        }
        self.payload.clear();
        self.raw_len = 0;
        self.state = State::Ground;
    }
}

/// 只做便宜的前缀检查：是否存在 `ESC P` / C1 DCS，或 chunk 以 ESC 结尾
/// （`ESC` 与 `P` 可能被 PTY 切成两块）。普通带颜色转义输出的 chunk 不会
/// 进入逐字节状态机。
fn contains_dcs_start(bytes: &[u8]) -> bool {
    if bytes.last() == Some(&ESC) || bytes.contains(&0x90) {
        return true;
    }
    bytes.windows(2).any(|w| w == [ESC, b'P'])
}

// ---------------------------------------------------------------- 解码

struct Row {
    idx: Vec<u8>,
    /// 覆盖位图：1 = 该像素被 Sixel 设置，0 = 透明。
    mask: Vec<u64>,
}

impl Default for Row {
    fn default() -> Self {
        Self { idx: Vec::new(), mask: Vec::new() }
    }
}

impl Row {
    fn set(&mut self, x: usize, color: u8) {
        if x >= MAX_DIMENSION {
            return;
        }
        if self.idx.len() <= x {
            self.idx.resize(x + 1, 0);
        }
        let word = x / 64;
        if self.mask.len() <= word {
            self.mask.resize(word + 1, 0);
        }
        self.mask[word] |= 1u64 << (x % 64);
        self.idx[x] = color;
    }

    #[inline]
    fn covered(&self, x: usize) -> bool {
        let word = x / 64;
        self.mask.get(word).is_some_and(|w| w & (1u64 << (x % 64)) != 0)
    }
}

#[derive(Default)]
struct Band {
    rows: [Row; 6],
}

#[derive(Clone, Copy)]
struct Palette {
    colors: [[u8; 3]; MAX_COLORS],
}

impl Default for Palette {
    fn default() -> Self {
        // 未定义时按黑色处理；绝大多数 Sixel 编码器（img2sixel/dsh/chafa）
        // 都会显式定义用到的寄存器。
        Self { colors: [[0, 0, 0]; MAX_COLORS] }
    }
}

impl Palette {
    fn define(&mut self, index: u8, system: u32, a: u32, b: u32, c: u32) {
        let rgb = if system == 1 {
            hls_to_rgb(a, b, c)
        } else {
            // P2=2 是 0..100 百分比；部分编码器直接发 0..255，按 >100 自动识别。
            let wide = a > 100 || b > 100 || c > 100;
            [rgb_channel(a, wide), rgb_channel(b, wide), rgb_channel(c, wide)]
        };
        self.colors[index as usize] = rgb;
    }
}

#[inline]
fn rgb_channel(value: u32, wide: bool) -> u8 {
    if wide {
        value.min(255) as u8
    } else {
        ((value.min(100) * 255 + 50) / 100) as u8
    }
}

/// HLS（H 0..=360，L/S 0..=100）→ RGB。
fn hls_to_rgb(h: u32, l: u32, s: u32) -> [u8; 3] {
    let h = (h % 360) as f32;
    let l = l.min(100) as f32 / 100.0;
    let s = s.min(100) as f32 / 100.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to_u8 = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    [to_u8(r1), to_u8(g1), to_u8(b1)]
}

/// 从 `i` 读一个十进制整数（饱和到 u32::MAX）。
fn read_u32(payload: &[u8], i: &mut usize) -> Option<u32> {
    let start = *i;
    let mut value = 0u32;
    while *i < payload.len() && payload[*i].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add((payload[*i] - b'0') as u32);
        *i += 1;
    }
    (*i > start).then_some(value)
}

#[inline]
fn is_sixel_char(b: u8) -> bool {
    (0x3f..=0x7e).contains(&b)
}

/// 解析并解码一条 Sixel payload（DCS 头与 ST 已去掉）。
///
/// 返回 `None` = 非法/超限/空图；调用方只丢图，不影响后续数据。
pub fn decode_sixel(payload: &[u8]) -> Option<SixelImage> {
    let mut palette = Palette::default();
    let mut bands: Vec<Band> = Vec::new();
    let mut current: u8 = 0;
    let mut x: usize = 0;
    let mut y: usize = 0;
    let mut declared: Option<(usize, usize)> = None;

    let mut i = 0usize;
    while i < payload.len() {
        let b = payload[i];
        match b {
            b'!' => {
                i += 1;
                let count = read_u32(payload, &mut i).unwrap_or(1).max(1);
                if i < payload.len() && is_sixel_char(payload[i]) {
                    let bits = payload[i] - 0x3f;
                    i += 1;
                    // 重复次数可能巨大；按剩余可用宽度截断，避免空转。
                    let count = (count as usize).min(MAX_DIMENSION.saturating_sub(x)).max(1);
                    paint(&mut bands, x, y, bits, current, count);
                    x = (x + count).min(MAX_DIMENSION);
                }
            },
            b'"' => {
                i += 1;
                let mut w = 0usize;
                let mut h = 0usize;
                for field in 0..4 {
                    let Some(value) = read_u32(payload, &mut i) else { break };
                    match field {
                        2 => w = value as usize,
                        3 => h = value as usize,
                        _ => {},
                    }
                    if i < payload.len() && payload[i] == b';' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if w > 0 && h > 0 {
                    if w > MAX_DIMENSION || h > MAX_DIMENSION || w.saturating_mul(h) > MAX_PIXELS
                    {
                        return None;
                    }
                    declared = Some((w, h));
                }
            },
            b'#' => {
                i += 1;
                let Some(register) = read_u32(payload, &mut i) else { continue };
                let mut params = [register, 0, 0, 0, 0];
                let mut count = 1usize;
                while count < 5 && i < payload.len() && payload[i] == b';' {
                    i += 1;
                    let Some(value) = read_u32(payload, &mut i) else { break };
                    params[count] = value;
                    count += 1;
                }
                // 跳过格式错误的额外参数，避免把它们当 sixel 数据画出来。
                while i < payload.len() && payload[i] == b';' {
                    i += 1;
                    if read_u32(payload, &mut i).is_none() {
                        break;
                    }
                }
                let index = register as usize;
                if index >= MAX_COLORS {
                    continue;
                }
                if count >= 5 {
                    palette.define(index as u8, params[1], params[2], params[3], params[4]);
                }
                current = index as u8;
            },
            b'$' => {
                x = 0;
                i += 1;
            },
            b'-' => {
                x = 0;
                y = y.saturating_add(6);
                if y / 6 > MAX_DIMENSION / 6 {
                    return None;
                }
                i += 1;
            },
            b'?'..=b'~' => {
                let bits = b - 0x3f;
                paint(&mut bands, x, y, bits, current, 1);
                x = (x + 1).min(MAX_DIMENSION);
                i += 1;
            },
            // 换行/空白/其它控制字节：忽略。
            _ => i += 1,
        }
    }

    // raster attributes 是编码器声明的最终画布尺寸：用它裁剪最后不满 6 px 的
    // band，也允许声明比实际绘制更大的透明边。没有声明时按实际绘制范围。
    let drawn_width = bands
        .iter()
        .flat_map(|band| band.rows.iter().map(|row| row.idx.len()))
        .max()
        .unwrap_or(0);
    let (width, height) = match declared {
        Some((w, h)) => (w, h),
        None => (drawn_width, bands.len() * 6),
    };
    if width == 0 || height == 0 {
        return None;
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION || width.saturating_mul(height) > MAX_PIXELS
    {
        return None;
    }

    let mut rgba = vec![0u8; width * height * 4];
    for (band_index, band) in bands.iter().enumerate() {
        for (bit, row) in band.rows.iter().enumerate() {
            let py = band_index * 6 + bit;
            if py >= height {
                continue;
            }
            for px in 0..row.idx.len().min(width) {
                if !row.covered(px) {
                    continue;
                }
                let [r, g, b] = palette.colors[row.idx[px] as usize];
                let off = (py * width + px) * 4;
                rgba[off] = r;
                rgba[off + 1] = g;
                rgba[off + 2] = b;
                rgba[off + 3] = 255;
            }
        }
    }

    Some(SixelImage { width: width as u32, height: height as u32, rgba })
}

fn paint(bands: &mut Vec<Band>, x: usize, y: usize, bits: u8, color: u8, count: usize) {
    let band_index = y / 6;
    if band_index > MAX_DIMENSION / 6 {
        return;
    }
    if bands.len() <= band_index {
        bands.resize_with(band_index + 1, Band::default);
    }
    let band = &mut bands[band_index];
    for offset in 0..count {
        let px = x + offset;
        if px >= MAX_DIMENSION {
            break;
        }
        for bit in 0..6 {
            if bits & (1 << bit) != 0 {
                band.rows[bit].set(px, color);
            }
        }
        // 某一行出现，说明该 band 至少在这一列有内容；同步放宽所有行长度，
        // 保证最终 width 取到最大绘制列（透明列也占位）。
        for row in &mut band.rows {
            if row.idx.len() <= px {
                row.idx.resize(px + 1, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(data: &[u8], decode: bool) -> (Vec<u8>, Vec<SixelImage>) {
        let mut filter = SixelFilter::new();
        let evs = filter.feed(data, decode).unwrap_or_default();
        let mut pass = Vec::new();
        let mut images = Vec::new();
        for ev in evs {
            match ev {
                SixelEvent::Pass(bytes) => pass.extend_from_slice(&bytes),
                SixelEvent::Image(img) => images.push(img),
            }
        }
        (pass, images)
    }

    /// 2x2 红/绿图案：第 1 行红、第 2 行绿；`#` 定义 RGB。
    fn sample_payload() -> Vec<u8> {
        // 每个 Sixel 字符覆盖 6 行；bit0+bit1 = `?`+3 = `B`，画 2x2。
        b"\"1;1;2;2#0;2;100;0;0#1;2;0;100;0#0B#1B".to_vec()
    }

    #[test]
    fn decodes_rgb_and_transparency() {
        let img = decode_sixel(&sample_payload()).expect("decode");
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(&img.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[4..8], &[0, 255, 0, 255]);
        let row2 = 2 * 4;
        assert_eq!(&img.rgba[row2..row2 + 4], &[255, 0, 0, 255]);
        assert_eq!(&img.rgba[row2 + 4..row2 + 8], &[0, 255, 0, 255]);
    }

    #[test]
    fn transparent_pixels_stay_alpha_zero() {
        // 只画左下角一个点；其余透明。
        let img = decode_sixel(b"#0;2;100;100;100#0@").expect("decode");
        assert_eq!((img.width, img.height), (1, 6));
        assert_eq!(img.rgba[0..4], [255, 255, 255, 255]);
        assert_eq!(img.rgba[4..8], [0, 0, 0, 0]);
    }

    #[test]
    fn hls_green() {
        let img = decode_sixel(b"#0;1;120;50;100#0@").expect("decode");
        assert_eq!(&img.rgba[0..3], &[0, 255, 0]);
    }

    #[test]
    fn repeat_and_raster_attributes() {
        // 4x1 蓝：!4 重复 `@`；raster 声明 4x1，只输出第一行。
        let img = decode_sixel(b"\"1;1;4;1#0;2;0;0;100!4@").expect("decode");
        assert_eq!((img.width, img.height), (4, 1));
        for i in 0..4 {
            assert_eq!(&img.rgba[i * 4..i * 4 + 3], &[0, 0, 255]);
        }
    }

    #[test]
    fn rejects_oversize_declared_raster() {
        assert!(decode_sixel(b"\"1;1;20000;2#0@").is_none());
        // 面积超 16 MP（5000x4000=20 MP）
        assert!(decode_sixel(b"\"1;1;5000;4000#0@").is_none());
    }

    #[test]
    fn rejects_empty_and_fuzzish_payloads() {
        assert!(decode_sixel(b"").is_none());
        assert!(decode_sixel(b"#0;2;1;2").is_none());
        // 截断的数字/重复符不会 panic。
        assert!(decode_sixel(b"!999999999999999999999999").is_none());
        let _ = decode_sixel(b"#9999999999;2;1;2;3@!;\"1;1;");
        let _ = decode_sixel(&[0xff; 128]);
    }

    #[test]
    fn filter_passes_normal_bytes_fast_path() {
        let mut filter = SixelFilter::new();
        assert!(filter.feed(b"hello\x1b[31mworld\x1b[0m", true).is_none());
    }

    #[test]
    fn filter_drops_sixel_and_keeps_st_prefix_suffix() {
        let mut payload = b"pre\x1bPq".to_vec();
        payload.extend_from_slice(&sample_payload());
        payload.extend_from_slice(b"\x1b\\post");
        let (pass, images) = events(&payload, true);
        assert_eq!(pass, b"prepost");
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (2, 2));
    }

    #[test]
    fn filter_accepts_bel_terminator() {
        let mut payload = b"\x1bPq".to_vec();
        payload.extend_from_slice(&sample_payload());
        payload.push(0x07);
        payload.extend_from_slice(b"after");
        let (pass, images) = events(&payload, true);
        assert_eq!(pass, b"after");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn filter_handles_chunk_boundaries_everywhere() {
        let mut payload = b"a\x1bP0;0;0q".to_vec();
        payload.extend_from_slice(&sample_payload());
        payload.extend_from_slice(b"\x1b\\b");
        for split in 0..payload.len() {
            let mut filter = SixelFilter::new();
            let mut pass = Vec::new();
            let mut images = 0;
            for (index, part) in [&payload[..split], &payload[split..]].into_iter().enumerate() {
                match filter.feed(part, true) {
                    // None = 本段没有 Sixel，调用方原样交给 vte。
                    None => {
                        if index == 0 || !part.is_empty() {
                            pass.extend_from_slice(part);
                        }
                    },
                    Some(evs) => {
                        for ev in evs {
                            match ev {
                                SixelEvent::Pass(p) => pass.extend_from_slice(&p),
                                SixelEvent::Image(_) => images += 1,
                            }
                        }
                    },
                }
            }
            assert_eq!(pass, b"ab", "split={split}");
            assert_eq!(images, 1, "split={split}");
        }
    }

    #[test]
    fn non_sixel_dcs_is_passed_through() {
        // DECRQSS `$q` 不是 Sixel，必须原样交给 alacritty。
        let data = b"\x1bP$q1;2;3\x1b\\x";
        let (pass, images) = events(data, true);
        assert_eq!(pass, data);
        assert!(images.is_empty());
    }

    #[test]
    fn decode_disabled_still_filters() {
        let mut payload = b"\x1bPq".to_vec();
        payload.extend_from_slice(&sample_payload());
        payload.push(0x07);
        payload.extend_from_slice(b"next");
        let (pass, images) = events(&payload, false);
        assert_eq!(pass, b"next");
        assert!(images.is_empty());
    }

    #[test]
    fn overflow_is_dropped_until_terminator() {
        let mut filter = SixelFilter::new();
        // 头部之后塞超限原始数据，再给 BEL 和后续文本。
        let mut chunk = b"\x1bPq#0;2;100;0;0".to_vec();
        chunk.extend(std::iter::repeat_n(b'@', MAX_RAW_BYTES + 64));
        chunk.extend_from_slice(b"\x07ok");
        let evs = filter.feed(&chunk, true).expect("scanned");
        let mut pass = Vec::new();
        let mut images = 0;
        for ev in evs {
            match ev {
                SixelEvent::Pass(p) => pass.extend_from_slice(&p),
                SixelEvent::Image(_) => images += 1,
            }
        }
        assert_eq!(pass, b"ok");
        assert_eq!(images, 0);
    }

    #[test]
    fn malformed_escape_inside_sixel_does_not_hang() {
        let mut filter = SixelFilter::new();
        let evs = filter
            .feed(b"\x1bPq#0;2;100;0;0@\x1bXafter", true)
            .expect("scanned");
        let mut pass = Vec::new();
        for ev in evs {
            if let SixelEvent::Pass(p) = ev {
                pass.extend_from_slice(&p);
            }
        }
        // 中止图片后 `after` 应恢复为普通文本。
        assert!(pass.ends_with(b"after"), "pass={pass:?}");
    }

    #[test]
    fn da1_rewrite_only_when_sixel_enabled() {
        assert_eq!(
            rewrite_da1("\x1b[?6c", true).as_deref(),
            Some("\x1b[?62;4;9;22c")
        );
        assert_eq!(rewrite_da1("\x1b[?6c", false), None);
        // 二级 DA / 其它设备属性绝不改写，避免误报。
        assert_eq!(rewrite_da1("\x1b[>0;1;2c", true), None);
        assert_eq!(rewrite_da1("\x1b[?62;4;9;22c", true), None);
    }
}

/// 把 alacritty 的 Primary DA 回应替换成带 Sixel 参数 `4` 的回应。
///
/// 只匹配 `ESC [ ? 6 c`（alacritty 0.26 的固定回复）；二级 DA（`ESC [ > ...`）
/// 与已经是六参数形式的回复原样返回 `None`。关闭 Sixel 时绝不添加参数 4。
pub fn rewrite_da1(reply: &str, sixel_enabled: bool) -> Option<String> {
    if !sixel_enabled || reply != "\x1b[?6c" {
        return None;
    }
    Some("\x1b[?62;4;9;22c".to_string())
}
