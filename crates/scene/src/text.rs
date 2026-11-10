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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextAttributes {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// The cell is selected. Its `foreground`/`background` already reflect selection colors.
    pub selected: bool,
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
