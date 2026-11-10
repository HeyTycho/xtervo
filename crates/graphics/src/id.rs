//! Stable identities used across the graphics and scene boundary.

/// Protocol-level image resource id.
///
/// An image id may be replaced with newer pixels, but only by re-inserting a resource with a
/// higher generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u32);

/// Immutable pixel-data identity.
///
/// A live [`SourceId`] always identifies exactly one generation of pixels. It is never reused
/// for different data while it is live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u64);

/// Identity of one visible placement.
///
/// Multiple placements may share one image source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlacementId(pub u64);

/// Stable terminal row identity provided by the vendored terminal grid.
///
/// Row numbers and scroll positions are not identities and must never be guessed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RowId(pub u64);
