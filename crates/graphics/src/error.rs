//! Validation errors for the graphics ownership model.

use std::fmt;

use crate::{ImageId, PlacementId, SourceId};

/// A rejected resource or placement mutation.
///
/// Validation is fallible rather than silently repairing state. A failed insertion leaves the
/// manager unchanged, which keeps protocol error reporting and row-ownership cleanup simple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphicsError {
    /// The resource has invalid dimensions or a pixel buffer of the wrong length.
    InvalidResource(&'static str),
    /// The source is already owned by a different live image id.
    SourceInUse {
        source_id: SourceId,
        image_id: ImageId,
    },
    /// A live source was offered with a different generation or different pixels.
    SourceConflict {
        image_id: ImageId,
        source_id: SourceId,
        reason: &'static str,
    },
    /// The replacement generation is not strictly newer than the live one.
    StaleGeneration {
        image_id: ImageId,
        current: u64,
        incoming: u64,
    },
    /// A placement names an image id that is not currently live.
    UnknownImage(ImageId),
    /// Placement ids are unique while live.
    DuplicatePlacement(PlacementId),
    /// A placement's source does not match the source of its image resource.
    SourceMismatch {
        placement_id: PlacementId,
        image_id: ImageId,
        expected: SourceId,
        actual: SourceId,
    },
    /// The placement geometry is not drawable.
    InvalidPlacement(&'static str),
    /// The requested source rectangle is outside the resource.
    SourceRectOutOfBounds {
        placement_id: PlacementId,
        image_id: ImageId,
    },
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResource(reason) => write!(f, "invalid image resource: {reason}"),
            Self::SourceInUse {
                source_id,
                image_id,
            } => write!(
                f,
                "source {:?} is already owned by image {:?}",
                source_id, image_id
            ),
            Self::SourceConflict {
                image_id,
                source_id,
                reason,
            } => write!(
                f,
                "image {:?} cannot reuse source {:?}: {reason}",
                image_id, source_id
            ),
            Self::StaleGeneration {
                image_id,
                current,
                incoming,
            } => write!(
                f,
                "image {:?} generation {incoming} is not newer than {current}",
                image_id
            ),
            Self::UnknownImage(image_id) => write!(f, "unknown image {image_id:?}"),
            Self::DuplicatePlacement(placement_id) => {
                write!(f, "placement {placement_id:?} already exists")
            }
            Self::SourceMismatch {
                placement_id,
                image_id,
                expected,
                actual,
            } => write!(
                f,
                "placement {:?} for image {:?} uses source {:?}, expected {:?}",
                placement_id, image_id, actual, expected
            ),
            Self::InvalidPlacement(reason) => write!(f, "invalid image placement: {reason}"),
            Self::SourceRectOutOfBounds {
                placement_id,
                image_id,
            } => write!(
                f,
                "placement {:?} source rectangle is outside image {:?}",
                placement_id, image_id
            ),
        }
    }
}

impl std::error::Error for GraphicsError {}
