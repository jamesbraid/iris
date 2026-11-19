//! The host-cursor glyph `reader.rs` hands the display, as iris's
//! `gfx_display` defines it.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwCursor {
    /// Glyph top-left in display coordinates; negative near the top/left edge.
    pub x: i32,
    pub y: i32,
    /// Glyph side, 32 or 64.
    pub size: usize,
    /// One word per glyph row, bit `c` set where column `c` is opaque.
    pub mask: Vec<u64>,
}
