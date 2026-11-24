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

impl SourceId {
    /// 把 pane/窗口命名空间拼进高位；本地 source id 只保留低 32 位。
    ///
    /// 每个 pane 的 ImageStore 都从 1 开始分配 source id，而 GPU 纹理注册表
    /// 是进程级共享的；不拼命名空间时两个 pane 的第一张图会撞车。
    pub const fn namespaced(namespace: u64, local: u64) -> Self {
        Self((namespace << 32) | (local & 0xffff_ffff))
    }
}

#[cfg(test)]
mod tests {
    use super::SourceId;

    #[test]
    fn namespaced_source_ids_do_not_collide_across_panes() {
        assert_eq!(SourceId::namespaced(1, 1), SourceId((1 << 32) | 1));
        assert_eq!(SourceId::namespaced(2, 1), SourceId((2 << 32) | 1));
        assert_ne!(SourceId::namespaced(1, 1), SourceId::namespaced(2, 1));
        // 本地 id 超 32 位时只保留低 32 位，命名空间不被污染。
        assert_eq!(
            SourceId::namespaced(1, 0x1_0000_0001),
            SourceId((1 << 32) | 1)
        );
    }
}

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
