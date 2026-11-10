//! Immutable image resources and their row-anchored placements.

use std::sync::Arc;

use crate::{GraphicsError, ImageId, PlacementId, Rect, RowId, SourceId};

/// Z layer used by terminal text (and therefore by cell selection styling).
pub const TEXT_LAYER_Z: i32 = 0;

/// Largest composition layer a "below text" image can occupy.
pub const BELOW_TEXT_LAYER_MAX_Z: i32 = -1;

/// Smallest composition layer an "above text" image can occupy.
pub const ABOVE_TEXT_LAYER_MIN_Z: i32 = 1;

/// Graphics protocol that produced a placement.
///
/// The manager itself is protocol-agnostic; this value is carried through to the renderer for
/// diagnostics and protocol-specific quirks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Sixel,
    Iterm2,
}

/// How an image composites against terminal text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositePolicy {
    /// Ordinary terminal images: background, image, glyphs.
    BelowText,
    /// Kitty `C=1` / positive z-index placement: image above glyphs.
    AboveText,
    /// Protocol declares the image owns its cells (placeholder/ImageCell).
    CellOwned,
}

impl CompositePolicy {
    /// Whether the policy puts the image above the text layer.
    pub const fn is_above_text(self) -> bool {
        matches!(self, Self::AboveText | Self::CellOwned)
    }

    /// Maps a protocol z-index to the scene layer used for composition.
    ///
    /// A protocol z-index is not a global order: the policy wins over the numeric value. This
    /// keeps every [`Self::BelowText`] image below the text layer and every
    /// [`Self::AboveText`] image above it, while preserving relative ordering within each group.
    pub const fn composition_z(self, protocol_z: i32) -> i32 {
        match self {
            Self::BelowText => {
                if protocol_z < BELOW_TEXT_LAYER_MAX_Z {
                    protocol_z
                } else {
                    BELOW_TEXT_LAYER_MAX_Z
                }
            }
            Self::AboveText | Self::CellOwned => {
                if protocol_z > ABOVE_TEXT_LAYER_MIN_Z {
                    protocol_z
                } else {
                    ABOVE_TEXT_LAYER_MIN_Z
                }
            }
        }
    }
}

/// Immutable decoded pixel data.
///
/// A resource can outlive a particular placement but never outlives the manager. Its
/// `source_id` identifies these exact pixels; an updated image gets a new source and a higher
/// generation.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageResource {
    pub id: ImageId,
    pub source_id: SourceId,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    pub generation: u64,
}

impl ImageResource {
    /// The full source-pixel rectangle.
    pub fn source_bounds(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width as f32, self.height as f32)
    }

    /// Expected RGBA buffer length, or `None` on integer overflow.
    pub fn expected_rgba_len(&self) -> Option<usize> {
        (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(4)
    }

    /// Validates dimensions and pixel-buffer length.
    ///
    /// Transfer-size limits (for example the 128 MiB decode budget) belong to the protocol
    /// decoder. The manager only enforces the structural invariant needed by the renderer.
    pub fn validate(&self) -> Result<(), GraphicsError> {
        if self.width == 0 || self.height == 0 {
            return Err(GraphicsError::InvalidResource(
                "width and height must be non-zero",
            ));
        }
        let expected = self
            .expected_rgba_len()
            .ok_or(GraphicsError::InvalidResource(
                "pixel dimensions overflow the address space",
            ))?;
        if self.rgba.len() != expected {
            return Err(GraphicsError::InvalidResource(
                "rgba byte length does not match width * height * 4",
            ));
        }
        Ok(())
    }
}

/// One visible placement of an image at a stable terminal row.
#[derive(Clone, Debug, PartialEq)]
pub struct ImagePlacement {
    pub id: PlacementId,
    pub image_id: ImageId,
    pub source_id: SourceId,
    pub row_id: RowId,
    pub col: u16,
    /// Crop in source pixels. `None` means the whole resource.
    pub source_rect: Option<Rect>,
    pub cells_w: u16,
    pub cells_h: u16,
    /// Pixel nudge relative to the cell-grid anchor.
    pub pixel_offset: (i32, i32),
    pub z_index: i32,
    pub protocol: ImageProtocol,
    pub policy: CompositePolicy,
}

impl ImagePlacement {
    /// Protocol-independent composition layer for this placement.
    pub const fn composition_z(&self) -> i32 {
        self.policy.composition_z(self.z_index)
    }

    /// Whether this placement draws above the text layer.
    pub const fn is_above_text(&self) -> bool {
        self.policy.is_above_text()
    }
}
