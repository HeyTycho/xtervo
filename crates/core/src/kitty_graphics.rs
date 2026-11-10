// SPDX-License-Identifier: GPL-3.0-only
//! Kitty graphics protocol: stream-safe APC filter + command parser.
//!
//! This module is deliberately standalone: it does not own a renderer, a
//! texture, or the screen model. [`KittyParser::feed`] consumes raw PTY bytes,
//! strips `ESC _ G ... ESC \` APCs, and emits backend-neutral
//! [`KittyCommand`]s plus the non-APC bytes that still belong to the VT parser.
//!
//! # Scope
//!
//! Implemented:
//!
//! - `a=q` (query), `a=t` (transmit), `a=T` (transmit + display),
//!   `a=p` (place), `a=d` (delete)
//! - control keys `i,p,c,r,x,y,w,h,z,C,q,m,f,s,v,o` (and `I` for image numbers)
//! - `f=32` RGBA and `f=24` RGB (expanded to RGBA), `o=z` zlib
//! - base64 chunking with `m=1`/`m=0`, including chunk state inheritance
//! - APC responses to queries and placement acknowledgements
//! - negative z-index and `C=1` (no cursor movement)
//! - bounded APC/image/control input, strict base64 and zlib validation
//!
//! Not implemented (explicitly rejected or ignored, never silently faked):
//!
//! - PNG `f=100`, indexed-color `f=8`/`f=16`
//! - file/shared-memory transmission (`t=f/t/s`), `S`/`O`
//! - animation (`a=f`, `a=a`, `a=c`), unicode placeholders, relative
//!   placements, pixel offsets `X`/`Y`
//!
//! # Integration mapping
//!
//! The output types intentionally do not depend on `tervo-graphics`, wgpu or
//! Slint. The integrator maps:
//!
//! ```text
//! ImageData        -> tervo_graphics::ImageResource (SourceId belongs to manager)
//! PlacementCommand -> tervo_graphics::ImagePlacement (attach current RowId)
//! DeleteCommand    -> GraphicsManager removal
//! Response(bytes)  -> write to the PTY
//! Error.response   -> write to the PTY when present
//! ```
//!
//! A placement command cannot carry a `RowId`: the protocol places relative to
//! the *current cursor*, so the screen/cursor owner has to attach the row
//! identity when it consumes the command. Anonymous images (`i=0`) are allowed
//! for `a=T`, but cannot be referenced later by `a=p`; the integrator should
//! place the image transmitted immediately before the command.
//!
//! # Limits
//!
//! - A single APC (control + base64 payload) is capped by [`MAX_APC_BYTES`].
//! - A single decoded image (RGBA, or RGB before expansion) is capped by
//!   [`MAX_IMAGE_BYTES`]; dimensions by [`MAX_DIMENSION`].
//! - The tracked image index is capped by [`MAX_IMAGES`] and
//!   [`MAX_TOTAL_IMAGE_BYTES`]. The index only stores metadata; pixel storage
//!   belongs to the graphics manager. If the manager evicts an image without a
//!   protocol delete, it should call [`KittyParser::forget_image`] so future
//!   `a=p` acknowledgements do not claim a resource that no longer exists.
//!
//! Invalid input never panics: it becomes [`KittyEvent::Error`] and, when the
//! protocol says a response is expected and `q` allows it, an APC response.

use std::collections::HashMap;
use std::io::Read;

use flate2::read::ZlibDecoder;

use crate::osc::decode_base64_bounded;

/// Maximum size of one APC body (control data + base64 payload, excluding
/// `ESC _ G` and the terminating `ESC \`).
pub const MAX_APC_BYTES: usize = 8 * 1024 * 1024;
/// Maximum decoded pixel data held for one image (RGBA bytes).
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum sum of decoded pixel data tracked in the parser's image index.
pub const MAX_TOTAL_IMAGE_BYTES: usize = 256 * 1024 * 1024;
/// Maximum width or height of an image.
pub const MAX_DIMENSION: u32 = 8192;
/// Maximum number of image ids tracked by the parser.
pub const MAX_IMAGES: usize = 4096;
/// Control-data (the part before `;`) limit; keeps key parsing bounded.
const MAX_CONTROL_BYTES: usize = 4096;
/// Maximum number of comma-separated control fields.
const MAX_CONTROL_FIELDS: usize = 64;

const ESC: u8 = 0x1b;
const C1_ST: u8 = 0x9c;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// One event produced by [`KittyParser::feed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KittyEvent {
    /// Bytes that do not belong to a Kitty APC and must be passed to the VT
    /// parser in this order.
    Pass(Vec<u8>),
    /// A recognized, fully validated protocol command.
    Command(KittyCommand),
    /// A protocol or input error. `response` contains the APC reply bytes to
    /// write back to the PTY when the protocol expects one.
    Error(KittyError),
}

/// Backend-neutral commands emitted by the parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KittyCommand {
    /// APC response bytes to write back to the PTY.
    Response(Vec<u8>),
    /// Store or replace image pixel data. The graphics manager owns the
    /// resource lifetime; `replaces_existing` means all old placements for the
    /// same image id must be dropped before storing this one.
    Transmit(TransmitCommand),
    /// Display a transmitted image at the current cursor cell.
    Place(PlacementCommand),
    /// Delete protocol targets. The manager resolves placement/cell targets.
    Delete(DeleteCommand),
}

/// Decoded pixel data for one image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageData {
    /// Protocol image id. `0` means anonymous.
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Tightly packed, non-premultiplied RGBA8 (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

/// A completed `a=t` / `a=T` transmission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransmitCommand {
    pub image: ImageData,
    /// True when this id already existed; old placements must be removed.
    pub replaces_existing: bool,
    /// Protocol quiet level (`0/1/2`) that produced this command.
    pub quiet: u8,
}

/// Source rectangle in image pixels for a placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Cursor movement policy after placing an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMovement {
    /// Move right/down by the placement rectangle (default, `C=0`).
    Move,
    /// Do not move the cursor (`C=1`).
    NoMove,
}

/// Backend-neutral compositing policy derived from the protocol z-index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositePolicy {
    /// Negative z-index: draw below cell backgrounds/glyphs.
    BelowText,
    /// Zero or positive z-index: draw above glyphs.
    AboveText,
}

/// A completed `a=p` / `a=T` display command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementCommand {
    pub image_id: u32,
    /// Protocol placement id. `0` means the terminal/manager chooses one.
    pub placement_id: u32,
    /// `None` means the full image.
    pub source_rect: Option<SourceRect>,
    /// Placement rectangle width in cells. `0` means auto.
    pub columns: u16,
    /// Placement rectangle height in cells. `0` means auto.
    pub rows: u16,
    /// Protocol z-index, including negative values and `i32::MIN`.
    pub z_index: i32,
    pub cursor_movement: CursorMovement,
    pub composite: CompositePolicy,
    pub quiet: u8,
}

/// A completed `a=d` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteCommand {
    pub target: DeleteTarget,
    pub quiet: u8,
}

/// Delete target, preserving the lowercase/uppercase data-freeing distinction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteTarget {
    /// `a=d` without a `d` key: all visible images/placements.
    All,
    /// `d=a/A`: all visible placements.
    AllPlacements { free_data: bool },
    /// `d=i/I`: all placements for an image, or one image+placement pair.
    Image {
        image_id: u32,
        placement_id: Option<u32>,
        free_data: bool,
    },
    /// `d=n/N`: newest image with the given image number (`I` key).
    NewestImage {
        image_number: u32,
        placement_id: Option<u32>,
        free_data: bool,
    },
    /// `d=c/C`: placements intersecting the current cursor cell.
    Cursor { free_data: bool },
    /// `d=p/P/q/Q`: placements intersecting a cell (optionally a z-index).
    Cell {
        x: u32,
        y: u32,
        z_index: Option<i32>,
        free_data: bool,
    },
    /// `d=x/X`: placements intersecting a column.
    Column { x: u32, free_data: bool },
    /// `d=y/Y`: placements intersecting a row.
    Row { y: u32, free_data: bool },
    /// `d=z/Z`: placements with a z-index.
    ZIndex { z_index: i32, free_data: bool },
    /// `d=r/R`: images whose id is in the inclusive range.
    ImageRange {
        min_image_id: u32,
        max_image_id: u32,
        free_data: bool,
    },
}

/// Kitty error code mapped to the protocol's ASCII status prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyErrorCode {
    Invalid,
    NoSuchImage,
    Unsupported,
    TooLarge,
    TooManyImages,
    BadBase64,
    BadZlib,
}

impl KittyErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "EINVAL",
            Self::NoSuchImage => "ENOENT",
            Self::Unsupported => "ENOTSUP",
            Self::TooLarge => "EINVAL",
            Self::TooManyImages => "ENOSPC",
            Self::BadBase64 => "EINVAL",
            Self::BadZlib => "EINVAL",
        }
    }
}

/// A parser error. `response` is `Some` only when a reply must be written to
/// the PTY (an image id was supplied or this was a query, and `q` allows it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyError {
    pub code: KittyErrorCode,
    pub detail: &'static str,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub response: Option<Vec<u8>>,
}

/// Stream-safe Kitty APC filter/parser.
///
/// Feed arbitrary PTY chunks; the parser keeps partial `ESC _ G` prefixes and
/// in-flight chunked transmissions across calls. All non-Kitty bytes are
/// returned as [`KittyEvent::Pass`].
#[derive(Default)]
pub struct KittyParser {
    scan: ScanState,
    pass: Vec<u8>,
    apc: Vec<u8>,
    pending: Option<PendingTransmission>,
    images: HashMap<u32, ImageMeta>,
    tracked_bytes: usize,
}

impl KittyParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a PTY chunk and return all events in stream order.
    pub fn feed(&mut self, input: &[u8]) -> Vec<KittyEvent> {
        let mut out = Vec::new();
        self.feed_into(input, &mut out);
        out
    }

    /// Feed a PTY chunk, appending events to `out`.
    pub fn feed_into(&mut self, input: &[u8], out: &mut Vec<KittyEvent>) {
        // Fast path: no ESC means no APC can start. Avoid a byte-by-byte copy
        // for ordinary output; the parser is expected to sit on the PTY hot
        // path.
        if self.scan == ScanState::Ground && self.pass.is_empty() && !input.contains(&ESC) {
            if !input.is_empty() {
                out.push(KittyEvent::Pass(input.to_vec()));
            }
            return;
        }

        for &b in input {
            match self.scan {
                ScanState::Ground => {
                    if b == ESC {
                        self.scan = ScanState::Escape;
                    } else {
                        self.pass.push(b);
                    }
                }
                ScanState::Escape => {
                    if b == b'_' {
                        self.scan = ScanState::ApcStart;
                    } else if b == ESC {
                        // `ESC ESC`: the first ESC is ordinary output, the
                        // second may still start an APC.
                        self.pass.push(ESC);
                    } else {
                        self.pass.push(ESC);
                        self.pass.push(b);
                        self.scan = ScanState::Ground;
                    }
                }
                ScanState::ApcStart => {
                    if b == b'G' {
                        self.apc.clear();
                        self.scan = ScanState::ApcCollect;
                    } else {
                        // Non-Kitty APC: pass it through untouched and stop
                        // looking for nested `ESC _ G` until its terminator.
                        self.pass.extend_from_slice(&[ESC, b'_']);
                        if b == ESC {
                            self.scan = ScanState::OtherApcEscape;
                        } else {
                            self.pass.push(b);
                            self.scan = if matches!(b, C1_ST | CAN | SUB) {
                                ScanState::Ground
                            } else {
                                ScanState::OtherApc
                            };
                        }
                    }
                }
                ScanState::ApcCollect => match b {
                    ESC => self.scan = ScanState::ApcEscape,
                    C1_ST => self.finish_apc(out),
                    CAN | SUB => self.abort_apc(),
                    _ => self.push_apc_byte(b),
                },
                ScanState::ApcEscape => {
                    if b == b'\\' {
                        self.finish_apc(out);
                    } else {
                        // A lone ESC inside the APC is payload; reprocess the
                        // current byte as APC content / a new terminator.
                        self.push_apc_byte(ESC);
                        if self.scan == ScanState::ApcOverflow {
                            if b == ESC {
                                self.scan = ScanState::ApcOverflowEscape;
                            } else if b == C1_ST {
                                self.finish_apc_overflow(out);
                            } else if matches!(b, CAN | SUB) {
                                self.abort_apc();
                            }
                        } else if b == ESC {
                            // Another ESC could be the start of `ESC \`.
                            self.scan = ScanState::ApcEscape;
                        } else if b == C1_ST {
                            self.finish_apc(out);
                        } else if matches!(b, CAN | SUB) {
                            self.abort_apc();
                        } else {
                            self.scan = ScanState::ApcCollect;
                            self.push_apc_byte(b);
                        }
                    }
                }
                ScanState::ApcOverflow => match b {
                    ESC => self.scan = ScanState::ApcOverflowEscape,
                    C1_ST => self.finish_apc_overflow(out),
                    CAN | SUB => self.abort_apc(),
                    _ => {}
                },
                ScanState::ApcOverflowEscape => {
                    if b == b'\\' {
                        self.finish_apc_overflow(out);
                    } else if b == ESC {
                        // Still waiting for the terminator.
                    } else if b == C1_ST {
                        self.finish_apc_overflow(out);
                    } else if matches!(b, CAN | SUB) {
                        self.abort_apc();
                    } else {
                        self.scan = ScanState::ApcOverflow;
                    }
                }
                ScanState::OtherApc => {
                    self.pass.push(b);
                    match b {
                        ESC => self.scan = ScanState::OtherApcEscape,
                        C1_ST | CAN | SUB => self.scan = ScanState::Ground,
                        _ => {}
                    }
                }
                ScanState::OtherApcEscape => {
                    if b == b'\\' {
                        self.pass.push(b);
                        self.scan = ScanState::Ground;
                    } else if b == ESC {
                        // ESC ESC: first is ordinary APC content, second may
                        // still start the terminator.
                        self.pass.push(ESC);
                    } else {
                        self.pass.push(ESC);
                        self.pass.push(b);
                        self.scan = if matches!(b, C1_ST | CAN | SUB) {
                            ScanState::Ground
                        } else {
                            ScanState::OtherApc
                        };
                    }
                }
            }
        }

        self.flush_pass(out);
    }

    /// True while a chunked transmission is waiting for `m=0`.
    pub fn has_pending_transmission(&self) -> bool {
        self.pending.is_some()
    }

    /// Dimensions of a tracked image id.
    pub fn image_dimensions(&self, id: u32) -> Option<(u32, u32)> {
        self.images.get(&id).map(|meta| (meta.width, meta.height))
    }

    pub fn tracked_image_count(&self) -> usize {
        self.images.len()
    }

    pub fn tracked_image_bytes(&self) -> usize {
        self.tracked_bytes
    }

    /// Forget an image id from the parser's acknowledgement index. Call this
    /// when the graphics manager evicts an image outside a protocol delete.
    pub fn forget_image(&mut self, id: u32) {
        if let Some(meta) = self.images.remove(&id) {
            self.tracked_bytes = self.tracked_bytes.saturating_sub(meta.bytes);
        }
    }

    /// Forget every tracked image id without touching the scan state.
    pub fn clear_image_index(&mut self) {
        self.images.clear();
        self.tracked_bytes = 0;
    }

    /// Reset all parser state (scan state, partial APC, pending chunks, image
    /// index). Does not emit anything.
    pub fn reset(&mut self) {
        self.scan = ScanState::Ground;
        self.pass.clear();
        self.apc.clear();
        self.pending = None;
        self.images.clear();
        self.tracked_bytes = 0;
    }

    fn push_apc_byte(&mut self, b: u8) {
        if self.apc.len() >= MAX_APC_BYTES {
            self.apc.clear();
            self.apc.shrink_to_fit();
            self.scan = ScanState::ApcOverflow;
            return;
        }
        self.apc.push(b);
    }

    fn flush_pass(&mut self, out: &mut Vec<KittyEvent>) {
        if !self.pass.is_empty() {
            out.push(KittyEvent::Pass(std::mem::take(&mut self.pass)));
        }
    }

    fn abort_apc(&mut self) {
        self.apc.clear();
        self.scan = ScanState::Ground;
        // A CAN/SUB inside a chunk aborts the upload; never resume it.
        self.pending = None;
    }

    fn finish_apc(&mut self, out: &mut Vec<KittyEvent>) {
        let apc = std::mem::take(&mut self.apc);
        self.scan = ScanState::Ground;
        self.flush_pass(out);
        self.handle_apc(&apc, out);
    }

    fn finish_apc_overflow(&mut self, out: &mut Vec<KittyEvent>) {
        self.apc.clear();
        self.scan = ScanState::Ground;
        self.flush_pass(out);
        let quiet = self.pending.as_ref().map_or(0, |p| p.meta.quiet);
        let (image_id, placement_id) = self
            .pending
            .as_ref()
            .map_or((0, 0), |p| (p.meta.image_id, p.meta.placement_id));
        self.pending = None;
        self.emit_error(
            KittyErrorCode::TooLarge,
            "APC exceeds limit",
            image_id,
            placement_id,
            quiet,
            false,
            out,
        );
    }

    fn handle_apc(&mut self, apc: &[u8], out: &mut Vec<KittyEvent>) {
        let (control_bytes, payload) = match apc.iter().position(|&b| b == b';') {
            Some(pos) => (&apc[..pos], &apc[pos + 1..]),
            None => (apc, &[][..]),
        };

        if control_bytes.len() > MAX_CONTROL_BYTES {
            self.emit_error(
                KittyErrorCode::Invalid,
                "control data too long",
                0,
                0,
                0,
                false,
                out,
            );
            return;
        }

        let control = match parse_control(control_bytes) {
            Ok(control) => control,
            Err(code) => {
                self.emit_error(code, "invalid control data", 0, 0, 0, false, out);
                return;
            }
        };

        let action = match control.action {
            None => None,
            Some(b't') => Some(Action::Transmit),
            Some(b'T') => Some(Action::TransmitDisplay),
            Some(b'p') => Some(Action::Place),
            Some(b'd') => Some(Action::Delete),
            Some(b'q') => Some(Action::Query),
            Some(_) => {
                self.emit_error(
                    KittyErrorCode::Unsupported,
                    "unsupported graphics action",
                    control.image_id.unwrap_or(0),
                    control.placement_id.unwrap_or(0),
                    control.quiet.unwrap_or(0),
                    false,
                    out,
                );
                return;
            }
        };

        match action {
            None => {
                if self.pending.is_some() {
                    self.continue_pending(control, payload, out);
                } else {
                    // `a` defaults to `t`.
                    self.start_transmission(Action::Transmit, control, payload, out);
                }
            }
            Some(Action::Transmit) => {
                self.abort_pending(out);
                self.start_transmission(Action::Transmit, control, payload, out);
            }
            Some(Action::TransmitDisplay) => {
                self.abort_pending(out);
                self.start_transmission(Action::TransmitDisplay, control, payload, out);
            }
            Some(Action::Place) => {
                self.abort_pending(out);
                self.handle_place(control, out);
            }
            Some(Action::Delete) => {
                self.abort_pending(out);
                self.handle_delete(control, out);
            }
            Some(Action::Query) => {
                self.abort_pending(out);
                self.handle_query(control, payload, out);
            }
        }
    }

    fn abort_pending(&mut self, out: &mut Vec<KittyEvent>) {
        if let Some(pending) = self.pending.take() {
            if let Some((code, detail)) = pending.failed {
                self.emit_error(
                    code,
                    detail,
                    pending.meta.image_id,
                    pending.meta.placement_id,
                    pending.meta.quiet,
                    false,
                    out,
                );
            }
        }
    }

    fn continue_pending(&mut self, control: Control, payload: &[u8], out: &mut Vec<KittyEvent>) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };

        // Kitty requires continuation chunks to carry only `m` (and optionally
        // `q`); everything else is inherited from the first chunk.
        if !control.is_continuation_only() {
            self.emit_error(
                KittyErrorCode::Invalid,
                "continuation chunk must only carry m/q",
                pending.meta.image_id,
                pending.meta.placement_id,
                pending.meta.quiet,
                false,
                out,
            );
            return;
        }

        let more = control.more.unwrap_or(false);
        if let Some(quiet) = control.quiet {
            pending.meta.quiet = quiet;
        }

        if pending.failed.is_none() {
            if let Err((code, detail)) = append_chunk(&mut pending.data, payload, more) {
                pending.data.clear();
                pending.failed = Some((code, detail));
            }
        }

        if more {
            self.pending = Some(pending);
        } else {
            self.finish_pending(pending, out);
        }
    }

    fn finish_pending(&mut self, pending: PendingTransmission, out: &mut Vec<KittyEvent>) {
        if let Some((code, detail)) = pending.failed {
            self.emit_error(
                code,
                detail,
                pending.meta.image_id,
                pending.meta.placement_id,
                pending.meta.quiet,
                false,
                out,
            );
            return;
        }
        self.complete_transmission(pending.meta, pending.display, pending.data, out);
    }

    fn start_transmission(
        &mut self,
        action: Action,
        control: Control,
        payload: &[u8],
        out: &mut Vec<KittyEvent>,
    ) {
        let quiet = control.quiet.unwrap_or(0);
        let image_id = control.image_id.unwrap_or(0);
        let placement_id = control.placement_id.unwrap_or(0);
        let more = control.more.unwrap_or(false);

        let shape = match validate_shape(&control) {
            Ok(shape) => shape,
            Err((code, detail)) => {
                self.start_failed_transmission(
                    more,
                    image_id,
                    placement_id,
                    quiet,
                    code,
                    detail,
                    out,
                );
                return;
            }
        };

        let meta = TxMeta::new(shape, image_id, placement_id, quiet);

        if image_id != 0 {
            if let Err((code, detail)) = self.check_image_quota(&meta) {
                self.start_failed_transmission(
                    more,
                    image_id,
                    placement_id,
                    quiet,
                    code,
                    detail,
                    out,
                );
                return;
            }
        }

        let display = if action == Action::TransmitDisplay {
            match build_placement(&control, image_id, shape.width, shape.height) {
                Ok(place) => Some(place),
                Err((code, detail)) => {
                    self.start_failed_transmission(
                        more,
                        image_id,
                        placement_id,
                        quiet,
                        code,
                        detail,
                        out,
                    );
                    return;
                }
            }
        } else {
            None
        };

        let mut data = Vec::new();
        let append = append_chunk(&mut data, payload, more);
        if let Err((code, detail)) = append {
            if more {
                self.pending = Some(PendingTransmission {
                    meta,
                    display,
                    data: Vec::new(),
                    failed: Some((code, detail)),
                });
            } else {
                self.emit_error(code, detail, image_id, placement_id, quiet, false, out);
            }
            return;
        }

        if more {
            self.pending = Some(PendingTransmission {
                meta,
                display,
                data,
                failed: None,
            });
        } else {
            self.complete_transmission(meta, display, data, out);
        }
    }

    fn start_failed_transmission(
        &mut self,
        more: bool,
        image_id: u32,
        placement_id: u32,
        quiet: u8,
        code: KittyErrorCode,
        detail: &'static str,
        out: &mut Vec<KittyEvent>,
    ) {
        if more {
            // Swallow the remaining chunks up to `m=0` so the stream stays in
            // sync, then report one error.
            self.pending = Some(PendingTransmission {
                meta: TxMeta::failed(image_id, placement_id, quiet),
                display: None,
                data: Vec::new(),
                failed: Some((code, detail)),
            });
        } else {
            self.emit_error(code, detail, image_id, placement_id, quiet, false, out);
        }
    }

    fn check_image_quota(&self, meta: &TxMeta) -> Result<(), (KittyErrorCode, &'static str)> {
        let replaced = self.images.get(&meta.image_id).copied();
        if replaced.is_none() && self.images.len() >= MAX_IMAGES {
            return Err((KittyErrorCode::TooManyImages, "too many images"));
        }
        let base = self
            .tracked_bytes
            .saturating_sub(replaced.map_or(0, |meta| meta.bytes));
        if base.saturating_add(meta.rgba_bytes) > MAX_TOTAL_IMAGE_BYTES {
            return Err((KittyErrorCode::TooLarge, "image quota exceeded"));
        }
        Ok(())
    }

    fn complete_transmission(
        &mut self,
        meta: TxMeta,
        display: Option<PlacementCommand>,
        data: Vec<u8>,
        out: &mut Vec<KittyEvent>,
    ) {
        let rgba = match decode_pixels(&meta, &data) {
            Ok(rgba) => rgba,
            Err((code, detail)) => {
                self.emit_error(
                    code,
                    detail,
                    meta.image_id,
                    meta.placement_id,
                    meta.quiet,
                    false,
                    out,
                );
                return;
            }
        };

        let replaces_existing = if meta.image_id != 0 {
            if let Some(old) = self.images.remove(&meta.image_id) {
                self.tracked_bytes = self.tracked_bytes.saturating_sub(old.bytes);
                true
            } else {
                false
            }
        } else {
            false
        };

        if meta.image_id != 0 {
            self.images.insert(
                meta.image_id,
                ImageMeta {
                    width: meta.width,
                    height: meta.height,
                    bytes: rgba.len(),
                },
            );
            self.tracked_bytes = self.tracked_bytes.saturating_add(rgba.len());
        }

        out.push(KittyEvent::Command(KittyCommand::Transmit(
            TransmitCommand {
                image: ImageData {
                    id: meta.image_id,
                    width: meta.width,
                    height: meta.height,
                    rgba,
                },
                replaces_existing,
                quiet: meta.quiet,
            },
        )));

        if let Some(place) = display {
            out.push(KittyEvent::Command(KittyCommand::Place(place)));
        }

        self.emit_ok(meta.image_id, meta.placement_id, meta.quiet, out);
    }

    fn handle_place(&mut self, control: Control, out: &mut Vec<KittyEvent>) {
        let quiet = control.quiet.unwrap_or(0);
        let image_id = control.image_id.unwrap_or(0);
        let placement_id = control.placement_id.unwrap_or(0);

        if image_id == 0 {
            self.emit_error(
                KittyErrorCode::Invalid,
                "image id required for placement",
                0,
                placement_id,
                quiet,
                false,
                out,
            );
            return;
        }

        let Some(meta) = self.images.get(&image_id).copied() else {
            self.emit_error(
                KittyErrorCode::NoSuchImage,
                "image not found",
                image_id,
                placement_id,
                quiet,
                false,
                out,
            );
            return;
        };

        match build_placement(&control, image_id, meta.width, meta.height) {
            Ok(place) => {
                out.push(KittyEvent::Command(KittyCommand::Place(place)));
                self.emit_ok(image_id, placement_id, quiet, out);
            }
            Err((code, detail)) => {
                self.emit_error(code, detail, image_id, placement_id, quiet, false, out);
            }
        }
    }

    fn handle_delete(&mut self, control: Control, out: &mut Vec<KittyEvent>) {
        let quiet = control.quiet.unwrap_or(0);
        let image_id = control.image_id.unwrap_or(0);
        let placement_id = control.placement_id.unwrap_or(0);
        let free_data = control
            .delete_spec
            .is_some_and(|spec| spec.is_ascii_uppercase());

        let target = match control.delete_spec {
            None => DeleteTarget::All,
            Some(b'a' | b'A') => DeleteTarget::AllPlacements { free_data },
            Some(b'i' | b'I') => {
                if image_id == 0 {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "image id required for delete",
                        0,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                }
                let placement = (placement_id != 0).then_some(placement_id);
                if free_data && placement.is_none() {
                    self.forget_image(image_id);
                }
                DeleteTarget::Image {
                    image_id,
                    placement_id: placement,
                    free_data,
                }
            }
            Some(b'n' | b'N') => {
                let Some(number) = control.image_number else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "image number required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::NewestImage {
                    image_number: number,
                    placement_id: (placement_id != 0).then_some(placement_id),
                    free_data,
                }
            }
            Some(b'c' | b'C') => DeleteTarget::Cursor { free_data },
            Some(b'p' | b'P') => {
                let (Some(x), Some(y)) = (control.src_x, control.src_y) else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "cell coordinates required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::Cell {
                    x,
                    y,
                    z_index: None,
                    free_data,
                }
            }
            Some(b'q' | b'Q') => {
                let (Some(x), Some(y), Some(z_index)) = (control.src_x, control.src_y, control.z)
                else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "cell and z-index required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::Cell {
                    x,
                    y,
                    z_index: Some(z_index),
                    free_data,
                }
            }
            Some(b'x' | b'X') => {
                let Some(x) = control.src_x else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "column required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::Column { x, free_data }
            }
            Some(b'y' | b'Y') => {
                let Some(y) = control.src_y else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "row required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::Row { y, free_data }
            }
            Some(b'z' | b'Z') => {
                let Some(z_index) = control.z else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "z-index required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                DeleteTarget::ZIndex { z_index, free_data }
            }
            Some(b'r' | b'R') => {
                let (Some(min_image_id), Some(max_image_id)) = (control.src_x, control.src_y)
                else {
                    self.emit_error(
                        KittyErrorCode::Invalid,
                        "image id range required for delete",
                        image_id,
                        placement_id,
                        quiet,
                        false,
                        out,
                    );
                    return;
                };
                if free_data && min_image_id <= max_image_id {
                    let ids: Vec<u32> = self
                        .images
                        .keys()
                        .copied()
                        .filter(|id| *id >= min_image_id && *id <= max_image_id)
                        .collect();
                    for id in ids {
                        self.forget_image(id);
                    }
                }
                DeleteTarget::ImageRange {
                    min_image_id,
                    max_image_id,
                    free_data,
                }
            }
            Some(b'f' | b'F') => {
                self.emit_error(
                    KittyErrorCode::Unsupported,
                    "animation frames not implemented",
                    image_id,
                    placement_id,
                    quiet,
                    false,
                    out,
                );
                return;
            }
            Some(_) => {
                self.emit_error(
                    KittyErrorCode::Invalid,
                    "unknown delete specifier",
                    image_id,
                    placement_id,
                    quiet,
                    false,
                    out,
                );
                return;
            }
        };

        out.push(KittyEvent::Command(KittyCommand::Delete(DeleteCommand {
            target,
            quiet,
        })));
    }

    fn handle_query(&mut self, control: Control, payload: &[u8], out: &mut Vec<KittyEvent>) {
        let quiet = control.quiet.unwrap_or(0);
        let image_id = control.image_id.unwrap_or(0);
        let placement_id = control.placement_id.unwrap_or(0);

        if control.more.unwrap_or(false) {
            self.emit_error(
                KittyErrorCode::Invalid,
                "query cannot be chunked",
                image_id,
                placement_id,
                quiet,
                true,
                out,
            );
            return;
        }

        let has_shape = control.format.is_some()
            || control.width.is_some()
            || control.height.is_some()
            || control.compression.is_some()
            || control.medium.is_some();

        // A bare `a=q` (no dummy image data) is a capability probe.
        if payload.is_empty() && !has_shape {
            if should_reply(quiet, true) {
                out.push(KittyEvent::Command(KittyCommand::Response(build_response(
                    (image_id != 0).then_some(image_id),
                    None,
                    "OK",
                ))));
            }
            return;
        }

        let shape = match validate_shape(&control) {
            Ok(shape) => shape,
            Err((code, detail)) => {
                self.emit_error(code, detail, image_id, placement_id, quiet, true, out);
                return;
            }
        };
        let meta = TxMeta::new(shape, image_id, placement_id, quiet);

        let mut data = Vec::new();
        if let Err((code, detail)) = append_chunk(&mut data, payload, false) {
            self.emit_error(code, detail, image_id, placement_id, quiet, true, out);
            return;
        }

        match decode_pixels(&meta, &data) {
            Ok(_) => {
                if should_reply(quiet, true) {
                    out.push(KittyEvent::Command(KittyCommand::Response(build_response(
                        (image_id != 0).then_some(image_id),
                        None,
                        "OK",
                    ))));
                }
            }
            Err((code, detail)) => {
                self.emit_error(code, detail, image_id, placement_id, quiet, true, out);
            }
        }
    }

    fn emit_ok(&self, image_id: u32, placement_id: u32, quiet: u8, out: &mut Vec<KittyEvent>) {
        if image_id == 0 || !should_reply(quiet, true) {
            return;
        }
        let placement = (placement_id != 0).then_some(placement_id);
        out.push(KittyEvent::Command(KittyCommand::Response(build_response(
            Some(image_id),
            placement,
            "OK",
        ))));
    }

    fn emit_error(
        &self,
        code: KittyErrorCode,
        detail: &'static str,
        image_id: u32,
        placement_id: u32,
        quiet: u8,
        allow_anonymous: bool,
        out: &mut Vec<KittyEvent>,
    ) {
        let wants_id = image_id != 0;
        let response = if (wants_id || allow_anonymous) && should_reply(quiet, false) {
            let status = format!("{}:{}", code.as_str(), detail);
            Some(build_response(
                wants_id.then_some(image_id),
                (placement_id != 0).then_some(placement_id),
                &status,
            ))
        } else {
            None
        };
        out.push(KittyEvent::Error(KittyError {
            code,
            detail,
            image_id: wants_id.then_some(image_id),
            placement_id: (placement_id != 0).then_some(placement_id),
            response,
        }));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    ApcStart,
    ApcCollect,
    ApcEscape,
    ApcOverflow,
    ApcOverflowEscape,
    /// A non-Kitty APC (`ESC _ X` where `X != G`): pass through untouched.
    OtherApc,
    OtherApcEscape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Transmit,
    TransmitDisplay,
    Place,
    Delete,
    Query,
}

#[derive(Clone, Copy)]
struct ImageMeta {
    width: u32,
    height: u32,
    bytes: usize,
}

#[derive(Clone, Copy)]
struct Shape {
    format: u32,
    width: u32,
    height: u32,
    compressed: bool,
    expected_bytes: usize,
    rgba_bytes: usize,
}

#[derive(Clone, Copy)]
struct TxMeta {
    image_id: u32,
    placement_id: u32,
    quiet: u8,
    format: u32,
    width: u32,
    height: u32,
    compressed: bool,
    expected_bytes: usize,
    rgba_bytes: usize,
}

impl TxMeta {
    fn new(shape: Shape, image_id: u32, placement_id: u32, quiet: u8) -> Self {
        Self {
            image_id,
            placement_id,
            quiet,
            format: shape.format,
            width: shape.width,
            height: shape.height,
            compressed: shape.compressed,
            expected_bytes: shape.expected_bytes,
            rgba_bytes: shape.rgba_bytes,
        }
    }

    fn failed(image_id: u32, placement_id: u32, quiet: u8) -> Self {
        Self {
            image_id,
            placement_id,
            quiet,
            format: 32,
            width: 0,
            height: 0,
            compressed: false,
            expected_bytes: 0,
            rgba_bytes: 0,
        }
    }
}

struct PendingTransmission {
    meta: TxMeta,
    /// For `a=T`, the placement built from the first chunk's control data.
    display: Option<PlacementCommand>,
    data: Vec<u8>,
    failed: Option<(KittyErrorCode, &'static str)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Control {
    action: Option<u8>,
    delete_spec: Option<u8>,
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    format: Option<u32>,
    medium: Option<u8>,
    width: Option<u32>,
    height: Option<u32>,
    compression: Option<u8>,
    more: Option<bool>,
    quiet: Option<u8>,
    columns: Option<u32>,
    rows: Option<u32>,
    src_x: Option<u32>,
    src_y: Option<u32>,
    src_w: Option<u32>,
    src_h: Option<u32>,
    z: Option<i32>,
    no_move: Option<bool>,
}

impl Control {
    fn is_continuation_only(&self) -> bool {
        self.action.is_none()
            && self.delete_spec.is_none()
            && self.image_id.is_none()
            && self.image_number.is_none()
            && self.placement_id.is_none()
            && self.format.is_none()
            && self.medium.is_none()
            && self.width.is_none()
            && self.height.is_none()
            && self.compression.is_none()
            && self.columns.is_none()
            && self.rows.is_none()
            && self.src_x.is_none()
            && self.src_y.is_none()
            && self.src_w.is_none()
            && self.src_h.is_none()
            && self.z.is_none()
            && self.no_move.is_none()
    }
}

fn parse_control(bytes: &[u8]) -> Result<Control, KittyErrorCode> {
    let mut control = Control::default();
    if bytes.is_empty() {
        return Ok(control);
    }

    let mut fields = 0usize;
    for part in bytes.split(|&b| b == b',') {
        fields += 1;
        if fields > MAX_CONTROL_FIELDS || part.is_empty() {
            return Err(KittyErrorCode::Invalid);
        }
        let eq = part
            .iter()
            .position(|&b| b == b'=')
            .ok_or(KittyErrorCode::Invalid)?;
        let key = &part[..eq];
        let value = &part[eq + 1..];
        if key.len() != 1 || value.is_empty() {
            return Err(KittyErrorCode::Invalid);
        }

        match key[0] {
            b'a' => set_char(&mut control.action, value)?,
            b'd' => set_char(&mut control.delete_spec, value)?,
            b'q' => {
                let quiet = parse_u32(value)?;
                if quiet > 2 {
                    return Err(KittyErrorCode::Invalid);
                }
                set(&mut control.quiet, quiet as u8)?;
            }
            b'i' => set(&mut control.image_id, parse_u32(value)?)?,
            b'I' => set(&mut control.image_number, parse_u32(value)?)?,
            b'p' => set(&mut control.placement_id, parse_u32(value)?)?,
            b'f' => set(&mut control.format, parse_u32(value)?)?,
            b't' => set_char(&mut control.medium, value)?,
            b's' => set(&mut control.width, parse_u32(value)?)?,
            b'v' => set(&mut control.height, parse_u32(value)?)?,
            b'o' => set_char(&mut control.compression, value)?,
            b'm' => {
                let more = parse_u32(value)?;
                if more > 1 {
                    return Err(KittyErrorCode::Invalid);
                }
                set(&mut control.more, more == 1)?;
            }
            b'c' => set(&mut control.columns, parse_u32(value)?)?,
            b'r' => set(&mut control.rows, parse_u32(value)?)?,
            b'x' => set(&mut control.src_x, parse_u32(value)?)?,
            b'y' => set(&mut control.src_y, parse_u32(value)?)?,
            b'w' => set(&mut control.src_w, parse_u32(value)?)?,
            b'h' => set(&mut control.src_h, parse_u32(value)?)?,
            b'z' => set(&mut control.z, parse_i32(value)?)?,
            b'C' => {
                let no_move = parse_u32(value)?;
                if no_move > 1 {
                    return Err(KittyErrorCode::Invalid);
                }
                set(&mut control.no_move, no_move == 1)?;
            }
            // `e` (feature query) and unknown keys are ignored on purpose;
            // unsupported features are reported only when they would change
            // rendering (formats/mediums/actions).
            _ => {}
        }
    }
    if control.image_id.is_some() && control.image_number.is_some() {
        return Err(KittyErrorCode::Invalid);
    }
    Ok(control)
}

fn set<T>(slot: &mut Option<T>, value: T) -> Result<(), KittyErrorCode> {
    if slot.is_some() {
        return Err(KittyErrorCode::Invalid);
    }
    *slot = Some(value);
    Ok(())
}

fn set_char(slot: &mut Option<u8>, value: &[u8]) -> Result<(), KittyErrorCode> {
    if value.len() != 1 {
        return Err(KittyErrorCode::Invalid);
    }
    set(slot, value[0])
}

fn parse_u32(value: &[u8]) -> Result<u32, KittyErrorCode> {
    let mut out = 0u32;
    for &b in value {
        if !b.is_ascii_digit() {
            return Err(KittyErrorCode::Invalid);
        }
        out = out
            .checked_mul(10)
            .and_then(|n| n.checked_add(u32::from(b - b'0')))
            .ok_or(KittyErrorCode::Invalid)?;
    }
    Ok(out)
}

fn parse_i32(value: &[u8]) -> Result<i32, KittyErrorCode> {
    let (negative, digits) = match value.first() {
        Some(b'-') => (true, &value[1..]),
        Some(_) => (false, value),
        None => return Err(KittyErrorCode::Invalid),
    };
    if digits.is_empty() {
        return Err(KittyErrorCode::Invalid);
    }
    let mut magnitude = 0u64;
    for &b in digits {
        if !b.is_ascii_digit() {
            return Err(KittyErrorCode::Invalid);
        }
        magnitude = magnitude
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(b - b'0')))
            .ok_or(KittyErrorCode::Invalid)?;
    }
    if negative {
        if magnitude > i32::MIN.unsigned_abs() as u64 {
            return Err(KittyErrorCode::Invalid);
        }
        Ok((0u64.wrapping_sub(magnitude)) as i64 as i32)
    } else {
        if magnitude > i32::MAX as u64 {
            return Err(KittyErrorCode::Invalid);
        }
        Ok(magnitude as i32)
    }
}

fn validate_shape(control: &Control) -> Result<Shape, (KittyErrorCode, &'static str)> {
    let medium = control.medium.unwrap_or(b'd');
    if medium != b'd' {
        return Err((
            KittyErrorCode::Unsupported,
            "transmission medium not implemented",
        ));
    }

    let format = control.format.unwrap_or(32);
    if !matches!(format, 24 | 32) {
        let detail = match format {
            100 => "PNG format not implemented",
            8 | 16 => "indexed color format not implemented",
            _ => "unsupported image format",
        };
        return Err((KittyErrorCode::Unsupported, detail));
    }

    let compressed = match control.compression {
        None => false,
        Some(b'z') => true,
        Some(_) => return Err((KittyErrorCode::Unsupported, "unsupported compression")),
    };

    let (Some(width), Some(height)) = (control.width, control.height) else {
        return Err((KittyErrorCode::Invalid, "image dimensions are required"));
    };
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err((KittyErrorCode::Invalid, "image dimensions out of range"));
    }

    let pixels = u64::from(width) * u64::from(height);
    if pixels > (MAX_IMAGE_BYTES / 4) as u64 {
        return Err((KittyErrorCode::TooLarge, "image data too large"));
    }
    let rgba_bytes = (pixels as usize)
        .checked_mul(4)
        .ok_or((KittyErrorCode::TooLarge, "image data too large"))?;
    let bytes_per_pixel = if format == 24 { 3usize } else { 4usize };
    let expected_bytes = (pixels as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or((KittyErrorCode::TooLarge, "image data too large"))?;

    Ok(Shape {
        format,
        width,
        height,
        compressed,
        expected_bytes,
        rgba_bytes,
    })
}

fn append_chunk(
    data: &mut Vec<u8>,
    payload: &[u8],
    more: bool,
) -> Result<(), (KittyErrorCode, &'static str)> {
    // Every non-final chunk must end on a base64 group boundary and must not
    // contain padding, otherwise the next chunk cannot be concatenated.
    if more && (payload.len() % 4 != 0 || payload.contains(&b'=')) {
        return Err((
            KittyErrorCode::BadBase64,
            "non-final base64 chunk is not aligned",
        ));
    }
    if payload.is_empty() {
        return Ok(());
    }

    let decoded = decode_base64_bounded(payload, MAX_IMAGE_BYTES, true)
        .ok_or((KittyErrorCode::BadBase64, "invalid base64"))?;
    let remaining = MAX_IMAGE_BYTES.saturating_sub(data.len());
    if decoded.len() > remaining {
        return Err((KittyErrorCode::TooLarge, "image data too large"));
    }
    data.extend_from_slice(&decoded);
    Ok(())
}

fn decode_pixels(meta: &TxMeta, data: &[u8]) -> Result<Vec<u8>, (KittyErrorCode, &'static str)> {
    let raw = if meta.compressed {
        inflate_zlib(data, MAX_IMAGE_BYTES).map_err(|error| match error {
            InflateError::TooLarge => (KittyErrorCode::TooLarge, "image data too large"),
            InflateError::BadData => (KittyErrorCode::BadZlib, "zlib decompression failed"),
        })?
    } else {
        data.to_vec()
    };

    if raw.len() != meta.expected_bytes {
        return Err((
            KittyErrorCode::Invalid,
            "pixel data size does not match dimensions",
        ));
    }

    match meta.format {
        32 => Ok(raw),
        24 => {
            let mut rgba = Vec::with_capacity(meta.rgba_bytes);
            for pixel in raw.chunks_exact(3) {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            Ok(rgba)
        }
        _ => Err((KittyErrorCode::Unsupported, "unsupported image format")),
    }
}

enum InflateError {
    TooLarge,
    BadData,
}

fn inflate_zlib(input: &[u8], max_bytes: usize) -> Result<Vec<u8>, InflateError> {
    if input.is_empty() {
        return Err(InflateError::BadData);
    }
    let decoder = ZlibDecoder::new(input);
    let mut limited = decoder.take(max_bytes as u64 + 1);
    let mut out = Vec::new();
    limited
        .read_to_end(&mut out)
        .map_err(|_| InflateError::BadData)?;
    if out.len() > max_bytes {
        return Err(InflateError::TooLarge);
    }
    Ok(out)
}

fn build_placement(
    control: &Control,
    image_id: u32,
    width: u32,
    height: u32,
) -> Result<PlacementCommand, (KittyErrorCode, &'static str)> {
    let columns = u16::try_from(control.columns.unwrap_or(0))
        .map_err(|_| (KittyErrorCode::Invalid, "column count out of range"))?;
    let rows = u16::try_from(control.rows.unwrap_or(0))
        .map_err(|_| (KittyErrorCode::Invalid, "row count out of range"))?;

    let x = control.src_x.unwrap_or(0);
    let y = control.src_y.unwrap_or(0);
    let requested_w = control.src_w.unwrap_or(0);
    let requested_h = control.src_h.unwrap_or(0);
    let source_rect = if x == 0 && y == 0 && requested_w == 0 && requested_h == 0 {
        None
    } else {
        if x >= width || y >= height {
            return Err((KittyErrorCode::Invalid, "source rectangle out of bounds"));
        }
        let w = if requested_w == 0 {
            width - x
        } else {
            requested_w
        };
        let h = if requested_h == 0 {
            height - y
        } else {
            requested_h
        };
        if w == 0
            || h == 0
            || x.checked_add(w).map_or(true, |right| right > width)
            || y.checked_add(h).map_or(true, |bottom| bottom > height)
        {
            return Err((KittyErrorCode::Invalid, "source rectangle out of bounds"));
        }
        Some(SourceRect { x, y, w, h })
    };

    let z_index = control.z.unwrap_or(0);
    let no_move = control.no_move.unwrap_or(false);
    Ok(PlacementCommand {
        image_id,
        placement_id: control.placement_id.unwrap_or(0),
        source_rect,
        columns,
        rows,
        z_index,
        cursor_movement: if no_move {
            CursorMovement::NoMove
        } else {
            CursorMovement::Move
        },
        composite: if z_index < 0 {
            CompositePolicy::BelowText
        } else {
            CompositePolicy::AboveText
        },
        quiet: control.quiet.unwrap_or(0),
    })
}

fn should_reply(quiet: u8, ok: bool) -> bool {
    match quiet {
        // Kitty's actual policy (and what dsh relies on): q=1 suppresses OK
        // replies but still reports failures; q=2 suppresses every reply.
        1 => !ok,
        2 => false,
        _ => true,
    }
}

fn build_response(image_id: Option<u32>, placement_id: Option<u32>, status: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + status.len());
    out.extend_from_slice(b"\x1b_G");
    if let Some(id) = image_id {
        out.extend_from_slice(b"i=");
        out.extend_from_slice(id.to_string().as_bytes());
        if let Some(placement) = placement_id {
            out.extend_from_slice(b",p=");
            out.extend_from_slice(placement.to_string().as_bytes());
        }
    }
    out.push(b';');
    out.extend_from_slice(status.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}
