//! Text cells and their resolved attributes.
//!
//! Selection is deliberately part of this batch: it is resolved into the cell foreground and
//! background colors, with [`TextAttributes::selected`] retaining the semantic flag. There is no
//! selection overlay draw item.

/// A run of text cells. Cells carry their resolved colors so the renderer never has to own
/// terminal selection state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextBatch {
    pub revision: u64,
    pub cells: Vec<TextCell>,
}

impl TextBatch {
    /// Creates an empty batch for the given scene revision.
    pub fn new(revision: u64) -> Self {
        Self {
            revision,
            cells: Vec::new(),
        }
    }

    /// Whether the batch contains no cells.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Appends one resolved cell.
    pub fn push(&mut self, cell: TextCell) {
        self.cells.push(cell);
    }
}

/// One terminal cell in a [`TextBatch`].
#[derive(Clone, Debug, PartialEq)]
pub struct TextCell {
    /// Viewport row index, not a stable terminal row id.
    pub row: u16,
    /// Grid column.
    pub col: u16,
    /// Grapheme cluster or text run to shape.
    pub text: String,
    /// Resolved foreground color (including selection foreground).
    pub foreground: u32,
    /// Resolved background color (including selection background).
    pub background: u32,
    pub attributes: TextAttributes,
}

/// Text attributes affecting shaping or rasterization.
///
/// These are semantic flags copied from the terminal frame, not a promise that a renderer
/// implements all of them yet. Selection and search are resolved into the cell colors by the
/// builder; the flags below are retained so a renderer can still distinguish "selected because
/// the user selected it" from "selected because a search matched".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextAttributes {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub double_underline: bool,
    pub strikethrough: bool,
    /// Faint / dimmed glyph. The legacy CPU renderer maps this to the bold face.
    pub dim: bool,
    /// The terminal hides the glyph (SGR 8), but the cell background is still painted.
    pub hidden: bool,
    /// The cell colors were swapped before being written to this batch (SGR 7).
    ///
    /// `foreground`/`background` already contain the final post-inverse values.
    pub inverse: bool,
    /// A double-width cell. Its glyph occupies this column and the next one.
    pub wide: bool,
    /// The right half of a [`Self::wide`] cell. Renderers must not shape this cell again.
    pub wide_spacer: bool,
    /// The cell is selected. Its `foreground`/`background` already reflect selection colors.
    pub selected: bool,
    /// The cell is a search hit. Its colors already reflect the search highlight.
    pub searched: bool,
}

#[cfg(test)]
mod tests {
    use super::{TextAttributes, TextBatch, TextCell};

    #[test]
    fn selection_is_a_cell_attribute() {
        let mut batch = TextBatch::new(3);
        batch.push(TextCell {
            row: 1,
            col: 2,
            text: "x".to_string(),
            foreground: 0x00ff_ffff,
            background: 0x0000_8800,
            attributes: TextAttributes {
                selected: true,
                ..TextAttributes::default()
            },
        });

        assert!(!batch.is_empty());
        assert!(batch.cells[0].attributes.selected);
    }
}
