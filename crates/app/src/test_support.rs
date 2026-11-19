// SPDX-License-Identifier: GPL-3.0-only
//! Shared test-only prerequisites.
//!
//! Pixel-diff tests used to `return` when no system monospace font was found,
//! which turned "font missing" into a green test (R6). A font is a hard
//! prerequisite for these tests, so this shared helper panics with an
//! actionable install hint instead of silently passing. The load path is the
//! product's fixed fixture chain (`FONT_SETS`: JetBrains Mono first, then
//! DejaVu Sans Mono), so a machine that can run the app can run the tests.

use std::rc::Rc;

use crate::glyph::FontSet;

/// Test font size used by the damage/image differential tests.
pub(crate) const TEST_FONT_SIZE: f32 = 15.0;

/// Loads the fixed test-font fixture or fails the calling test loudly.
///
/// Use this in every pixel/damage differential test. Never replace it with
/// `let Some(fonts) = ... else { return; }`: a missing font must be red, not a
/// skipped (false-green) test.
pub(crate) fn require_test_fonts() -> Rc<FontSet> {
    if let Some(fonts) = FontSet::load(TEST_FONT_SIZE) {
        return Rc::new(fonts);
    }
    panic!(
        "render damage/image diff tests require a system monospace font; \
         install the pinned test fixture (for example `apt install \
         fonts-dejavu-core` or `fonts-jetbrains-mono`) so one of these exists:\n\
         - /usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Medium.ttf\n\
         - /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf\n\
         refusing to skip: a font-less run must fail, not pass without testing"
    );
}
