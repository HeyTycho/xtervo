// SPDX-License-Identifier: GPL-3.0-only
//! iTerm2 内联图片协议（IIP，`OSC 1337;File=...:BASE64`）：流式过滤 + PNG/JPEG 解码。
//!
//! 设计要点：
//! - **旁路过滤**：IIP 是 OSC 字符串，base64 载荷可达数十 MB。这里像 Sixel 一样
//!   先把整条序列从 PTY 字节流里摘掉，只把其余字节按原顺序交给 vte；图片事件
//!   夹在前后文本之间，actor 收到时读到的光标就是图片锚点。
//! - **跨 chunk**：`ESC ] 1337;File=` 前缀、参数、base64、BEL/ST 都可以被 PTY
//!   读缓冲切断；状态机跨 `feed` 调用保持。
//! - **严格解码**：base64 拒绝非法字符、错误长度/补位和非规范的尾部未用位；
//!   解码后的文件必须是 PNG/JPEG 且通过尺寸校验。
//! - **安全上限**：单个文件 20 MiB、单图 16 MP / 单边 16384 px。非法/超限数据
//!   只丢图（序列本身仍被吞掉），绝不 panic、绝不让图片字节进入网格。
//! - **尺寸参数**：`inline=1`、`name`、`width`/`height`（cell 数、`px`、`auto`）、
//!   `preserveAspectRatio`。未声明 `inline=1` 的文件只下载不显示，因此直接忽略。
//!
//! 解码输出是**非预乘** RGBA（未覆盖的像素 alpha=0），由 Slint 统一预乘。

use std::io::Cursor;

use crate::osc::decode_base64_bounded;

/// 单张图片文件（base64 解码后）的字节上限：20 MiB。
pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;
/// 单图像素上限（16 MP），与 Sixel 保持一致的预算。
pub const MAX_PIXELS: usize = 16 * 1024 * 1024;
/// 单图单边像素上限。
pub const MAX_DIMENSION: u32 = 16_384;
/// `File=` 参数区的上限：正常参数只有几十字节，这里留足 `name` 的空间。
const MAX_ARGS_BYTES: usize = 16 * 1024;
/// 单条 OSC 1337 缓冲上限：base64 编码 20 MiB 的正文 + 参数余量。
const MAX_BODY_BYTES: usize = 4 * MAX_FILE_BYTES.div_ceil(3) + MAX_ARGS_BYTES;
/// 大缓冲用完就释放，避免每个显示过图片的 pane 长期常驻 ~28 MiB。
const RELEASE_BODY_CAPACITY: usize = 256 * 1024;
/// `OSC 1337;File=` 中 `File=` 之后的固定前缀（OSC 编号与分号已单独匹配）。
const PREFIX: &[u8] = b"1337;File=";

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const C1_ST: u8 = 0x9c;
const C1_OSC: u8 = 0x9d;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8] = &[0xff, 0xd8, 0xff];

/// 一张已解码、已按显示尺寸缩放的 IIP 图片（直通 RGBA8）。
pub struct IipImage {
    /// 显示像素宽高（已应用 width/height/preserveAspectRatio，可能不等于原图）。
    pub width: u32,
    pub height: u32,
    /// 直通 RGBA8，长度 = width * height * 4。
    pub rgba: Vec<u8>,
    /// `name` 参数（base64 解码后的文件名）；仅用于诊断，不参与布局。
    pub name: Option<String>,
}

impl std::fmt::Debug for IipImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IipImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .field("name", &self.name)
            .finish()
    }
}

/// 过滤结果：交给 vte 的普通字节，或一张已按光标锚定的图片，顺序与输入一致。
pub enum IipEvent {
    /// 原样交给 vte / OscScanner 的字节（已剔除 IIP OSC）。
    Pass(Vec<u8>),
    /// 一张解码成功的图片；锚点由 actor 在收到该事件时读取当前光标。
    Image(IipImage),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    /// 看到 ESC，等待 `]`（或判定不是 OSC）。
    Escape,
    /// 已看到 `ESC ]` / C1 OSC，正在匹配 `1337;File=`。
    Prefix,
    /// 正在收集 `File=` 之后的参数与 base64，直到 BEL/ST。
    Body,
    /// body 中看到 ESC，等待 `\` 组成 ST。
    BodyEscape,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SizeSpec {
    #[default]
    Auto,
    Cells(u32),
    Px(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileArgs {
    inline: bool,
    width: SizeSpec,
    height: SizeSpec,
    preserve_aspect_ratio: bool,
    /// 已解码的 `name`；非法 base64 不影响图片显示。
    name: Option<String>,
}

impl Default for FileArgs {
    fn default() -> Self {
        Self {
            inline: false,
            width: SizeSpec::Auto,
            height: SizeSpec::Auto,
            preserve_aspect_ratio: true,
            name: None,
        }
    }
}

/// 流式 `OSC 1337;File=` 过滤器。
pub struct IipFilter {
    state: State,
    /// 尚未决定去留的字节（ESC / C1 OSC / 前缀匹配失败时的暂存）。
    pending: Vec<u8>,
    /// 已匹配的 `1337;File=` 前缀；匹配失败时原样还给 vte。
    prefix: Vec<u8>,
    /// `File=` 之后的参数 + `:` + base64。
    body: Vec<u8>,
    /// body 超限后仍吞到终止符，但不再保存/解码。
    overflow: bool,
    /// 本次 feed 是否吞掉过字节；决定返回 None（原样放行）还是 Some。
    swallowed: bool,
    /// 进入 body 时的开关快照：关闭时只吞序列，不占内存。
    decode: bool,
    cell_w: u32,
    cell_h: u32,
}

impl Default for IipFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl IipFilter {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            pending: Vec::new(),
            prefix: Vec::new(),
            body: Vec::new(),
            overflow: false,
            swallowed: false,
            decode: false,
            cell_w: 8,
            cell_h: 16,
        }
    }

    /// 设置终端 cell 像素尺寸；`width`/`height` 以 cell 为单位时按此换算。
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        self.cell_w = u32::from(width.max(1));
        self.cell_h = u32::from(height.max(1));
    }

    /// 扫描一段 PTY 字节。
    ///
    /// - 返回 `None`：本段不含 IIP/悬空 OSC 前缀，调用方应把**原始** `bytes`
    ///   交给 vte。
    /// - 返回 `Some(events)`：按顺序处理 events；`Pass` 字节交给 vte，`Image`
    ///   表示此刻光标处新增一张图片。`Some(vec![])` 也是有效的（整段都是
    ///   IIP payload / 正在等前缀的下一个字节，不能把原文交给 vte）。
    ///
    /// `decode = false`（IIP 关闭）时仍会过滤/吞掉 OSC 1337 File，只是不做
    /// 解码与渲染，避免关闭配置后大段 base64 进入 vte 的 OSC 状态机。
    pub fn feed(&mut self, bytes: &[u8], decode: bool) -> Option<Vec<IipEvent>> {
        if self.state == State::Ground && self.pending.is_empty() && !contains_iip_start(bytes) {
            return None;
        }

        let mut out: Vec<IipEvent> = Vec::new();
        let mut pass: Vec<u8> = Vec::new();
        self.swallowed = false;

        for &b in bytes {
            match self.state {
                State::Ground => match b {
                    ESC => {
                        self.pending.push(ESC);
                        self.state = State::Escape;
                    },
                    C1_OSC => {
                        self.pending.push(C1_OSC);
                        self.prefix.clear();
                        self.state = State::Prefix;
                    },
                    _ => pass.push(b),
                },
                State::Escape => match b {
                    b']' => {
                        self.pending.push(b']');
                        self.prefix.clear();
                        self.state = State::Prefix;
                    },
                    // 连续 ESC：前一个不是 OSC，放行；当前 ESC 继续等待。
                    ESC => {
                        self.flush_pending(&mut pass);
                        self.pending.push(ESC);
                    },
                    _ => {
                        self.flush_pending(&mut pass);
                        self.replay_ground(b, &mut pass);
                    },
                },
                State::Prefix => {
                    if b == PREFIX[self.prefix.len()] {
                        self.prefix.push(b);
                        if self.prefix.len() == PREFIX.len() {
                            // 命中 IIP：丢弃 `ESC ]` 与 `File=`，开始收集正文。
                            self.pending.clear();
                            self.prefix.clear();
                            self.body.clear();
                            self.overflow = false;
                            self.decode = decode;
                            self.swallowed = true;
                            self.state = State::Body;
                        }
                    } else {
                        // 只是普通 OSC：把暂存的字节原样还回去。当前字节仍按
                        // Ground 语义处理，避免把紧跟在空 OSC 后的 ESC/C1 OSC
                        // 当成普通正文而漏掉下一张图片。
                        //
                        // 若当前字节本身就是 ESC/C1 OSC，说明前一个 OSC 是空的、
                        // 这两个字节属于下一条转义序列。空 OSC 没有可见内容，直接
                        // 丢弃前缀，避免把未终止的 OSC 交给 vte 后吞掉后续文本。
                        if b != ESC && b != C1_OSC {
                            pass.extend_from_slice(&self.pending);
                            pass.extend_from_slice(&self.prefix);
                        }
                        self.pending.clear();
                        self.prefix.clear();
                        self.state = State::Ground;
                        self.replay_ground(b, &mut pass);
                    }
                },
                State::Body => match b {
                    ESC => {
                        self.pending.push(ESC);
                        self.state = State::BodyEscape;
                    },
                    BEL | C1_ST => {
                        self.flush_pass(&mut pass, &mut out);
                        self.finish(false, &mut out);
                        self.state = State::Ground;
                    },
                    // CAN/SUB 中止 OSC：丢弃整张图，但不把它当终止符解码。
                    CAN | SUB => {
                        self.flush_pass(&mut pass, &mut out);
                        self.finish(true, &mut out);
                        self.state = State::Ground;
                    },
                    _ => self.push_body(b),
                },
                State::BodyEscape => match b {
                    b'\\' => {
                        self.flush_pass(&mut pass, &mut out);
                        self.finish(false, &mut out);
                        self.pending.clear();
                        self.state = State::Ground;
                    },
                    // 畸形 OSC（ESC 后不是 `\`）：丢弃图片，把 ESC 与当前字节交还
                    // 给 vte，让后续转义序列仍按原语义执行。
                    _ => {
                        self.body.clear();
                        self.overflow = false;
                        self.pending.clear();
                        pass.push(ESC);
                        pass.push(b);
                        self.state = State::Ground;
                    },
                },
            }
        }

        self.flush_pass(&mut pass, &mut out);
        if self.swallowed || !out.is_empty() || !self.pending.is_empty() || self.state != State::Ground
        {
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

    /// 把累积的 pass 字节作为一个事件按顺序提交（图片事件之前必须调用，
    /// 否则同一 chunk 里图片前的文本会被排到图片之后）。
    #[inline]
    fn flush_pass(&mut self, pass: &mut Vec<u8>, out: &mut Vec<IipEvent>) {
        if !pass.is_empty() {
            out.push(IipEvent::Pass(std::mem::take(pass)));
        }
    }

    /// ESC 后不是 `]`：ESC 已放行，这里处理它后面的字节。
    fn replay_ground(&mut self, b: u8, pass: &mut Vec<u8>) {
        match b {
            ESC => {
                self.pending.push(ESC);
                self.state = State::Escape;
            },
            C1_OSC => {
                self.pending.push(C1_OSC);
                self.prefix.clear();
                self.state = State::Prefix;
            },
            _ => pass.push(b),
        }
    }

    fn push_body(&mut self, b: u8) {
        if !self.decode || self.overflow {
            return;
        }
        if self.body.len() >= MAX_BODY_BYTES {
            // 超限后不再保存，继续吞到终止符，保证不污染网格。
            self.overflow = true;
            self.body.clear();
            self.body.shrink_to_fit();
            return;
        }
        self.body.push(b);
    }

    fn finish(&mut self, aborted: bool, out: &mut Vec<IipEvent>) {
        if !aborted && self.decode && !self.overflow {
            if let Some(image) = decode_body(&self.body, self.cell_w, self.cell_h) {
                out.push(IipEvent::Image(image));
            }
        }
        self.body.clear();
        if self.body.capacity() > RELEASE_BODY_CAPACITY {
            // 一张 20 MiB 图片的 base64 缓冲没必要长期占着；下次重新分配即可。
            self.body = Vec::new();
        }
        self.overflow = false;
    }
}

/// 只做便宜的前缀检查：普通带颜色转义输出的 chunk 不进入逐字节状态机。
fn contains_iip_start(bytes: &[u8]) -> bool {
    if bytes.last() == Some(&ESC) || bytes.contains(&C1_OSC) {
        return true;
    }
    bytes.windows(2).any(|w| w == [ESC, b']'])
}

/// 解析 `File=` 之后的参数区（不含 `:` 与 base64）。
fn parse_file_args(raw: &[u8]) -> Option<FileArgs> {
    if raw.len() > MAX_ARGS_BYTES {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    let mut args = FileArgs::default();
    for part in text.split(';') {
        if part.is_empty() {
            continue;
        }
        let (key, value) = part.split_once('=')?;
        match key.trim().to_ascii_lowercase().as_str() {
            "inline" => args.inline = parse_switch(value)?,
            "width" => args.width = parse_size(value)?,
            "height" => args.height = parse_size(value)?,
            "preserveaspectratio" => args.preserve_aspect_ratio = parse_switch(value)?,
            "name" => {
                // name 是 base64 文件名；解不出来不影响图片本身。
                args.name = decode_base64_bounded(value.trim().as_bytes(), 4096, false)
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .filter(|name| !name.is_empty());
            },
            // size/align/pad 等不影响本实现的布局；未知参数按协议忽略。
            _ => {},
        }
    }
    // iTerm2 协议里 inline 缺省为 0：只下载、不显示。没有 inline=1 就不是内联图。
    args.inline.then_some(args)
}

fn parse_switch(value: &str) -> Option<bool> {
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

fn parse_size(value: &str) -> Option<SizeSpec> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("auto") {
        return Some(SizeSpec::Auto);
    }
    let lower = value.to_ascii_lowercase();
    let (number, px) = match lower.strip_suffix("px") {
        Some(number) => (number, true),
        None => (lower.as_str(), false),
    };
    let number: u32 = number.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some(if px { SizeSpec::Px(number) } else { SizeSpec::Cells(number) })
}

/// 参数 + `:` + base64 → 已缩放的 RGBA 图片。
fn decode_body(body: &[u8], cell_w: u32, cell_h: u32) -> Option<IipImage> {
    decode_body_limited(body, cell_w, cell_h, MAX_FILE_BYTES)
}

fn decode_body_limited(
    body: &[u8],
    cell_w: u32,
    cell_h: u32,
    max_file_bytes: usize,
) -> Option<IipImage> {
    let colon = body.iter().position(|&b| b == b':')?;
    let (args_raw, payload) = body.split_at(colon);
    let payload = payload.get(1..)?;
    if payload.is_empty() {
        return None;
    }
    let args = parse_file_args(args_raw)?;
    // 严格 base64：拒绝非法字符/长度/补位以及非零的尾部未用位。
    let file = decode_base64_bounded(payload, max_file_bytes, true)?;
    let (natural_w, natural_h, rgba) = decode_image(&file)?;
    let (width, height) = fit_size(
        natural_w,
        natural_h,
        args.width,
        args.height,
        args.preserve_aspect_ratio,
        cell_w,
        cell_h,
    );
    let rgba = resize_rgba(&rgba, natural_w, natural_h, width, height);
    if rgba.len() != width as usize * height as usize * 4 {
        return None;
    }
    Some(IipImage { width, height, rgba, name: args.name })
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && u64::from(width) * u64::from(height) <= MAX_PIXELS as u64
}

fn decode_image(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if data.starts_with(PNG_MAGIC) {
        decode_png(data)
    } else if data.starts_with(JPEG_MAGIC) {
        decode_jpeg(data)
    } else {
        None
    }
}

fn decode_png(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    // 默认 64 MiB 对 16 MP RGBA + 解码行缓冲来说太紧；尺寸本身另有硬校验。
    decoder.set_limits(png::Limits { bytes: MAX_PIXELS * 4 + 8 * 1024 * 1024 });
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = reader.info().size();
    if !valid_dimensions(width, height) {
        return None;
    }
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight || color == png::ColorType::Indexed {
        return None;
    }
    let samples = color.samples();
    let size = reader.output_buffer_size()?;
    let expected = width as usize * height as usize * samples;
    if size < expected || size > MAX_PIXELS * 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf).ok()?;
    if frame.buffer_size() < expected {
        return None;
    }
    let rgba = to_rgba(&buf[..expected], color, width as usize * height as usize)?;
    Some((width, height, rgba))
}

fn decode_jpeg(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = zune_jpeg::JpegDecoder::new(Cursor::new(data));
    decoder.decode_headers().ok()?;
    let info = decoder.info()?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if !valid_dimensions(width, height) {
        return None;
    }
    let color = decoder.output_colorspace()?;
    let components = color.num_components();
    if !(1..=4).contains(&components) {
        return None;
    }
    let size = decoder.output_buffer_size()?;
    let expected = width as usize * height as usize * components;
    if size != expected || size > MAX_PIXELS * 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    decoder.decode_into(&mut buf).ok()?;
    let rgba = jpeg_to_rgba(&buf, color, width as usize * height as usize)?;
    Some((width, height, rgba))
}

/// zune-jpeg 默认输出 RGB；这里同时兼容灰度/BGR/带 alpha 的输出配置。
fn jpeg_to_rgba(
    buf: &[u8],
    color: zune_jpeg::zune_core::colorspace::ColorSpace,
    pixels: usize,
) -> Option<Vec<u8>> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    let channels = color.num_components();
    let src = buf.get(..pixels.checked_mul(channels)?)?;
    let mut out = Vec::with_capacity(pixels * 4);
    for pixel in src.chunks_exact(channels) {
        match color {
            ColorSpace::RGB => out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]),
            ColorSpace::RGBA => out.extend_from_slice(&[pixel[0], pixel[1], pixel[2], pixel[3]]),
            ColorSpace::BGR => out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]),
            ColorSpace::BGRA => out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]),
            ColorSpace::ARGB => out.extend_from_slice(&[pixel[1], pixel[2], pixel[3], pixel[0]]),
            ColorSpace::Luma => out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
            ColorSpace::LumaA => {
                out.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
            },
            _ => return None,
        }
    }
    Some(out)
}

fn to_rgba(buf: &[u8], color: png::ColorType, pixels: usize) -> Option<Vec<u8>> {
    match color {
        png::ColorType::Rgba => {
            let bytes = pixels.checked_mul(4)?;
            buf.get(..bytes).map(|s| s.to_vec())
        },
        png::ColorType::Rgb => expand(buf, pixels, 3, &[255]),
        png::ColorType::Grayscale => expand_gray(buf, pixels, 1),
        png::ColorType::GrayscaleAlpha => expand_gray(buf, pixels, 2),
        png::ColorType::Indexed => None,
    }
}

fn expand(buf: &[u8], pixels: usize, channels: usize, alpha: &[u8]) -> Option<Vec<u8>> {
    let src = pixels.checked_mul(channels)?;
    let src = buf.get(..src)?;
    let mut out = Vec::with_capacity(pixels * 4);
    for pixel in src.chunks_exact(channels) {
        out.extend_from_slice(pixel);
        out.extend_from_slice(alpha);
    }
    Some(out)
}

fn expand_gray(buf: &[u8], pixels: usize, channels: usize) -> Option<Vec<u8>> {
    let src = pixels.checked_mul(channels)?;
    let src = buf.get(..src)?;
    let mut out = Vec::with_capacity(pixels * 4);
    for pixel in src.chunks_exact(channels) {
        let gray = pixel[0];
        let alpha = if channels == 2 { pixel[1] } else { 255 };
        out.extend_from_slice(&[gray, gray, gray, alpha]);
    }
    Some(out)
}

/// 计算显示像素尺寸；`preserve_aspect_ratio` 语义与 iTerm2 一致。
fn fit_size(
    natural_w: u32,
    natural_h: u32,
    width: SizeSpec,
    height: SizeSpec,
    preserve_aspect_ratio: bool,
    cell_w: u32,
    cell_h: u32,
) -> (u32, u32) {
    let target = |spec: SizeSpec, cell: u32| -> Option<u64> {
        match spec {
            SizeSpec::Auto => None,
            SizeSpec::Px(px) => Some(u64::from(px)),
            SizeSpec::Cells(cells) => Some(u64::from(cells) * u64::from(cell.max(1))),
        }
    };
    let target_w = target(width, cell_w);
    let target_h = target(height, cell_h);
    let (natural_w, natural_h) = (u64::from(natural_w.max(1)), u64::from(natural_h.max(1)));

    let (mut w, mut h) = if preserve_aspect_ratio {
        let scale = match (target_w, target_h) {
            (None, None) => 1.0,
            (Some(w), None) => w as f64 / natural_w as f64,
            (None, Some(h)) => h as f64 / natural_h as f64,
            // 同时指定时按“装进目标框”处理，保持宽高比。
            (Some(w), Some(h)) => {
                (w as f64 / natural_w as f64).min(h as f64 / natural_h as f64)
            },
        };
        (
            (natural_w as f64 * scale).round().max(1.0) as u64,
            (natural_h as f64 * scale).round().max(1.0) as u64,
        )
    } else {
        (target_w.unwrap_or(natural_w), target_h.unwrap_or(natural_h))
    };

    w = w.clamp(1, u64::from(MAX_DIMENSION));
    h = h.clamp(1, u64::from(MAX_DIMENSION));
    if w.saturating_mul(h) > MAX_PIXELS as u64 {
        // 目标框本身超预算：等比缩到 16 MP 以内（不改变宽高比）。
        let scale = (MAX_PIXELS as f64 / (w as f64 * h as f64)).sqrt();
        w = ((w as f64 * scale).floor() as u64).max(1);
        h = ((h as f64 * scale).floor() as u64).max(1);
    }
    (w as u32, h as u32)
}

/// 双线性缩放 RGBA8（整数权重 0..=256）。尺寸相同时返回原数据。
fn resize_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let expected = src_w as usize * src_h as usize * 4;
    if src.len() != expected || dst_w == 0 || dst_h == 0 {
        return Vec::new();
    }
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }

    let axis = |dst: u32, src: u32| -> Vec<(usize, usize, u32)> {
        (0..dst)
            .map(|i| {
                let pos = ((i as f64 + 0.5) * src as f64 / dst as f64 - 0.5)
                    .clamp(0.0, src as f64 - 1.0);
                let low = pos.floor() as usize;
                let high = (low + 1).min(src as usize - 1);
                let weight = ((pos - low as f64) * 256.0).round() as u32;
                (low, high, weight)
            })
            .collect()
    };
    let xs = axis(dst_w, src_w);
    let ys = axis(dst_h, src_h);
    let mut out = vec![0u8; dst_w as usize * dst_h as usize * 4];

    for (dy, &(y0, y1, wy)) in ys.iter().enumerate() {
        let row0 = y0 * src_w as usize * 4;
        let row1 = y1 * src_w as usize * 4;
        let w_y0 = 256 - wy;
        for (dx, &(x0, x1, wx)) in xs.iter().enumerate() {
            let w_x0 = 256 - wx;
            let p00 = row0 + x0 * 4;
            let p10 = row0 + x1 * 4;
            let p01 = row1 + x0 * 4;
            let p11 = row1 + x1 * 4;
            let w00 = w_x0 * w_y0;
            let w10 = wx * w_y0;
            let w01 = w_x0 * wy;
            let w11 = wx * wy;
            let o = (dy * dst_w as usize + dx) * 4;
            for channel in 0..4 {
                let value = (u32::from(src[p00 + channel]) * w00
                    + u32::from(src[p10 + channel]) * w10
                    + u32::from(src[p01 + channel]) * w01
                    + u32::from(src[p11 + channel]) * w11
                    + 32_768)
                    >> 16;
                out[o + channel] = value.min(255) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::encode_base64;

    /// 2x2 PNG（RGBA）：红、绿、蓝、半透明白。
    fn rgba_png() -> Vec<u8> {
        let pixels: [u8; 16] = [
            255, 0, 0, 255, // 红
            0, 255, 0, 255, // 绿
            0, 0, 255, 255, // 蓝
            255, 255, 255, 128, // 半透明白
        ];
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(&pixels).expect("png data");
        }
        out
    }

    fn rgb_png(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![0x40u8; width as usize * height as usize * 3];
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer.write_image_data(&pixels).expect("png data");
        }
        out
    }

    /// 8x8 纯红 JPEG（4:4:4，quality 95）。
    const RED_JPEG_B64: &str = concat!(
        "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAIBAQEBAQIBAQECAgICAgQDAgICAgUEBAMEBgUGBgYF",
        "BgYGBwkIBgcJBwYGCAsICQoKCgoKBggLDAsKDAkKCgr/2wBDAQICAgICAgUDAwUKBwYHCgoKCgoK",
        "CgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgr/wAARCAAIAAgDARE",
        "AAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBA",
        "QAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NT",
        "Y3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpK",
        "Wmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHw",
        "EAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBS",
        "ExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElK",
        "U1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uL",
        "m6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwD4vr+Uz/fw",
        "/9k="
    );

    fn red_jpeg() -> Vec<u8> {
        crate::osc::decode_base64_bounded(RED_JPEG_B64.as_bytes(), 4096, false)
            .expect("jpeg fixture base64")
    }

    fn image_osc(args: &str, file: &[u8]) -> Vec<u8> {
        let mut out = format!("\x1b]1337;File={args}:").into_bytes();
        out.extend_from_slice(encode_base64(file).as_bytes());
        out.push(BEL);
        out
    }

    fn feed_chunks(chunks: &[&[u8]], decode: bool) -> (Vec<u8>, Vec<IipImage>) {
        let mut filter = IipFilter::new();
        filter.set_cell_size(10, 20);
        let mut pass = Vec::new();
        let mut images = Vec::new();
        for chunk in chunks {
            match filter.feed(chunk, decode) {
                // None = 本段没有 IIP，调用方应把原始字节交给 vte。
                None => pass.extend_from_slice(chunk),
                Some(events) => {
                    for event in events {
                        match event {
                            IipEvent::Pass(bytes) => pass.extend_from_slice(&bytes),
                            IipEvent::Image(image) => images.push(image),
                        }
                    }
                },
            }
        }
        (pass, images)
    }

    #[test]
    fn rgb_png_is_expanded_to_rgba() {
        let (pass, images) = feed_chunks(&[&image_osc("inline=1", &rgb_png(2, 2))], true);
        assert!(pass.is_empty());
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (2, 2));
        assert_eq!(images[0].rgba.len(), 16);
        assert_eq!(&images[0].rgba[0..4], &[0x40, 0x40, 0x40, 255]);
    }

    #[test]
    fn inline_png_is_decoded_and_keeps_surrounding_bytes_in_order() {
        let png = rgba_png();
        let osc = image_osc("inline=1;name=cGljLnBuZw==", &png);
        let mut input = b"before\n".to_vec();
        input.extend_from_slice(&osc);
        input.extend_from_slice(b"after\n");

        let (pass, images) = feed_chunks(&[&input], true);
        assert_eq!(pass, b"before\nafter\n");
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.rgba.len(), 16);
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255], "红色像素");
        assert_eq!(&image.rgba[4..8], &[0, 255, 0, 255], "绿色像素");
        assert_eq!(&image.rgba[12..16], &[255, 255, 255, 128], "半透明白");
        assert_eq!(image.name.as_deref(), Some("pic.png"));
    }

    #[test]
    fn iip_prefix_body_and_terminator_cross_chunks() {
        let png = rgba_png();
        let osc = image_osc("inline=1", &png);
        // 拆到每个字节都单独 feed：前缀、base64、BEL 全部跨 chunk。
        let chunks: Vec<&[u8]> = osc.chunks(1).collect();
        let (pass, images) = feed_chunks(&chunks, true);
        assert!(pass.is_empty());
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].width, images[0].height), (2, 2));

        // 只拆在关键边界：ESC | ] | "1337;File=" | args | base64 | ST。
        let mut with_text = b"X".to_vec();
        with_text.extend_from_slice(&osc);
        with_text.extend_from_slice(b"Y");
        let splits = [1, 2, 13, 26, osc.len() - 1];
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut start = 0;
        for split in splits {
            chunks.push(&with_text[start..split]);
            start = split;
        }
        chunks.push(&with_text[start..]);
        let (pass, images) = feed_chunks(&chunks, true);
        assert_eq!(pass, b"XY");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn st_c1_st_and_abort_terminators() {
        for terminator in [b"\x1b\\".as_slice(), &[C1_ST], b"\x07"] {
            let mut osc = image_osc("inline=1", &rgba_png());
            osc.pop(); // 去掉 BEL
            osc.extend_from_slice(terminator);
            let (pass, images) = feed_chunks(&[&osc], true);
            assert!(pass.is_empty(), "terminator={terminator:?}");
            assert_eq!(images.len(), 1, "terminator={terminator:?}");
        }
        // CAN/SUB 中止：丢图，不崩溃，后续文本仍是文本。
        for terminator in [&[CAN], &[SUB]] {
            let mut osc = b"left".to_vec();
            osc.extend_from_slice(&image_osc("inline=1", &rgba_png()));
            osc.pop();
            osc.extend_from_slice(terminator);
            osc.extend_from_slice(b"right");
            let (pass, images) = feed_chunks(&[&osc], true);
            assert_eq!(pass, b"leftright");
            assert!(images.is_empty());
        }
    }

    #[test]
    fn empty_osc_followed_by_iip_is_still_detected() {
        // 畸形但可安全处理的输入：`ESC ]` 后立即出现新的 `ESC ] 1337;File=`。
        let png = rgba_png();
        let osc = image_osc("inline=1", &png);
        let mut input = vec![ESC, b']'];
        input.extend_from_slice(&osc);
        let (pass, images) = feed_chunks(&[&input], true);
        assert!(pass.is_empty(), "空 OSC 前缀不应把 vte 留在 OSC 状态");
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn non_file_osc_and_non_inline_file_pass_or_drop_without_image() {
        // 非 File 的 OSC 1337 原样交给 vte。
        let set_user_var = b"\x1b]1337;SetUserVar=foo=YmFy\x07";
        let (pass, images) = feed_chunks(&[set_user_var], true);
        assert_eq!(pass, set_user_var);
        assert!(images.is_empty());

        // inline 缺省/为 0：不是内联图，不显示（序列本身仍被吞掉）。
        for args in ["", "inline=0", "name=Zm9v"] {
            let (pass, images) = feed_chunks(&[&image_osc(args, &rgba_png())], true);
            assert!(pass.is_empty(), "args={args}");
            assert!(images.is_empty(), "args={args}");
        }
    }

    #[test]
    fn invalid_base64_drops_image_without_breaking_output() {
        let bad_payloads: [&[u8]; 6] = [b"!!!!", b"A", b"AB=C", b"====", b"aGVs bG8=", b"YQ==YQ=="];
        for bad in bad_payloads {
            let mut input = b"<".to_vec();
            input.extend_from_slice(b"\x1b]1337;File=inline=1:");
            input.extend_from_slice(bad);
            input.extend_from_slice(b"\x07>");
            let (pass, images) = feed_chunks(&[&input], true);
            assert_eq!(pass, b"<>", "bad payload {bad:?}");
            assert!(images.is_empty(), "bad payload {bad:?}");
        }
        // 严格模式：补位字节的低位必须为 0（非规范编码）。
        assert!(decode_base64_bounded(b"AB==", 8, true).is_none());
        assert!(decode_base64_bounded(b"ABC=", 8, true).is_none());
        assert!(decode_base64_bounded(b"YQ==", 8, true).is_some());
        assert!(decode_base64_bounded(b"YWI=", 8, true).is_some());
    }

    #[test]
    fn oversize_file_and_args_are_dropped() {
        let png = rgba_png();
        let args = format!("inline=1;name=cGljLnBuZw==:{}", encode_base64(&png));
        assert!(
            decode_body_limited(args.as_bytes(), 10, 20, 8).is_none(),
            "文件字节超过 max_file_bytes 必须丢图"
        );

        let long_args = format!("inline=1;{}:{}", "x".repeat(MAX_ARGS_BYTES), encode_base64(&png));
        let (pass, images) = feed_chunks(&[&image_osc(&long_args, &[])], true);
        assert!(pass.is_empty());
        assert!(images.is_empty(), "参数区超限必须丢图");

        // 尺寸上限在解码前拦截：单边 16384 / 16 MP。
        assert!(!valid_dimensions(MAX_DIMENSION + 1, 1));
        assert!(!valid_dimensions(4096, 4097));
        assert!(valid_dimensions(4096, 4096));
    }

    #[test]
    fn oversize_body_is_consumed_without_retaining() {
        // 远大于 MAX_BODY_BYTES 的 base64 垃圾：状态机应持续吞到 BEL，不保存/不崩溃。
        let mut filter = IipFilter::new();
        let mut out = Vec::new();
        let head = b"\x1b]1337;File=inline=1:";
        out.extend(filter.feed(head, true).unwrap_or_default());
        let junk = vec![b'A'; 1024 * 1024];
        let mut fed = 0usize;
        while !filter.overflow {
            let events = filter.feed(&junk, true).unwrap_or_default();
            assert!(events.iter().all(|e| matches!(e, IipEvent::Pass(_))));
            fed += junk.len();
            assert!(fed <= MAX_BODY_BYTES + junk.len(), "超限后应尽快停止保存");
        }
        assert!(fed > MAX_BODY_BYTES, "body 应在 20 MiB 文件编码上限附近溢出");
        assert!(filter.body.is_empty(), "超限后不应继续保留 body");
        // 溢出后继续吞到终止符，不再保存。
        let _ = filter.feed(&junk, true);
        assert!(filter.body.is_empty());
        let events = filter.feed(b"\x07tail", true).unwrap_or_default();
        let mut pass = Vec::new();
        for event in events {
            if let IipEvent::Pass(bytes) = event {
                pass.extend_from_slice(&bytes);
            }
        }
        assert_eq!(pass, b"tail");
    }

    #[test]
    fn size_args_cell_px_auto_and_aspect_ratio() {
        let args = parse_file_args(
            b"inline=1;width=10;height=5px;preserveAspectRatio=0;name=cGljLnBuZw==",
        )
        .expect("args");
        assert!(args.inline);
        assert_eq!(args.width, SizeSpec::Cells(10));
        assert_eq!(args.height, SizeSpec::Px(5));
        assert!(!args.preserve_aspect_ratio);
        assert_eq!(args.name.as_deref(), Some("pic.png"));

        // cell = 10x20：20 列宽 → 200px 宽，等比高 100px。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Cells(20), SizeSpec::Auto, true, 10, 20),
            (200, 100)
        );
        // 50px 宽 → 高 25px。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Px(50), SizeSpec::Auto, true, 10, 20),
            (50, 25)
        );
        // 只给高度 5 行 = 100px → 宽 200px。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Auto, SizeSpec::Cells(5), true, 10, 20),
            (200, 100)
        );
        // 同时给 40x40 的框：等比装进 → 40x20。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Px(40), SizeSpec::Px(40), true, 10, 20),
            (40, 20)
        );
        // preserveAspectRatio=0：拉伸到目标框。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Px(40), SizeSpec::Px(40), false, 10, 20),
            (40, 40)
        );
        // 两个 auto：保持原图像素尺寸。
        assert_eq!(
            fit_size(100, 50, SizeSpec::Auto, SizeSpec::Auto, true, 10, 20),
            (100, 50)
        );
        // 目标超 16384 / 16 MP：等比压回预算内。
        let (w, h) = fit_size(100, 50, SizeSpec::Px(999_999), SizeSpec::Auto, true, 10, 20);
        assert!(w <= MAX_DIMENSION && h <= MAX_DIMENSION);
        assert!(u64::from(w) * u64::from(h) <= MAX_PIXELS as u64);
        assert_eq!(parse_size("auto"), Some(SizeSpec::Auto));
        assert_eq!(parse_size("12PX"), Some(SizeSpec::Px(12)));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("12.5"), None);
    }

    #[test]
    fn truncated_or_unknown_image_formats_are_dropped_without_panic() {
        let png = rgba_png();
        let jpeg = red_jpeg();
        assert!(decode_image(&png[..png.len() / 2]).is_none(), "截断 PNG 丢弃");
        assert!(decode_image(&jpeg[..jpeg.len() / 2]).is_none(), "截断 JPEG 丢弃");
        assert!(decode_image(b"GIF89a.....").is_none(), "不支持的格式丢弃");
        assert!(decode_image(&[0xff, 0xd8, 0xff, 0, 1, 2]).is_none(), "畸形 JPEG 丢弃");
        assert!(decode_image(b"\x89PNG\r\n\x1a\nnot-a-png").is_none(), "畸形 PNG 丢弃");
        // 随机垃圾喂给两个解码器都只返回 None。
        for seed in 0u16..32 {
            let junk: Vec<u8> = (0..256).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed as u8)).collect();
            let _ = decode_png(&junk);
            let _ = decode_jpeg(&junk);
        }
    }

    #[test]
    fn jpeg_inline_decodes_to_rgba() {
        let jpeg = red_jpeg();
        let (pass, images) = feed_chunks(&[&image_osc("inline=1", &jpeg)], true);
        assert!(pass.is_empty());
        assert_eq!(images.len(), 1);
        let image = &images[0];
        assert_eq!((image.width, image.height), (8, 8));
        assert_eq!(image.rgba.len(), 8 * 8 * 4);
        // JPEG 有损：纯红应接近 (255, 0, 0)。
        assert!(image.rgba[0] > 200, "r={}", image.rgba[0]);
        assert!(image.rgba[1] < 60, "g={}", image.rgba[1]);
        assert!(image.rgba[2] < 60, "b={}", image.rgba[2]);
    }

    #[test]
    fn decode_disabled_strips_iip_and_keeps_text() {
        let mut input = b"A".to_vec();
        input.extend_from_slice(&image_osc("inline=1", &rgba_png()));
        input.extend_from_slice(b"B");
        let (pass, images) = feed_chunks(&[&input], false);
        assert_eq!(pass, b"AB");
        assert!(images.is_empty());
    }

    #[test]
    fn malformed_escape_inside_iip_does_not_hang() {
        let mut input = b"\x1b]1337;File=inline=1;width=2".to_vec();
        input.extend_from_slice(b"\x1b[31mRED");
        let (pass, images) = feed_chunks(&[&input], true);
        assert!(images.is_empty());
        assert_eq!(pass, b"\x1b[31mRED");
    }

    #[test]
    fn osc_prefix_split_between_esc_and_bracket() {
        let png = rgba_png();
        let osc = image_osc("inline=1", &png);
        let (pass, images) = feed_chunks(&[b"\x1b", b"]1337;File=", &osc[12..]], true);
        assert!(pass.is_empty());
        assert_eq!(images.len(), 1);
    }
}
