//! Shared hash for the composed-frame pins of the graphics boards.
//!
//! Each board's test builds a fixed scene, runs its own scan-out into a plain
//! buffer and compares this hash against a constant. A pin that moves means
//! the pixels a display would show changed.

/// blake3 of the frame size and its `u32` words, little-endian, row by row.
/// `stride` is the distance between rows in `px`, in words.
pub(crate) fn frame_hash(w: usize, h: usize, stride: usize, px: &[u32]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(w as u32).to_le_bytes());
    hasher.update(&(h as u32).to_le_bytes());
    for row in px.chunks(stride).take(h) {
        for p in &row[..w] {
            hasher.update(&p.to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Number of distinct words in the visible part of the frame, for the
/// not-blank check.
pub(crate) fn colour_count(w: usize, h: usize, stride: usize, px: &[u32]) -> usize {
    let mut seen = std::collections::HashSet::new();
    for row in px.chunks(stride).take(h) {
        seen.extend(row[..w].iter().copied());
    }
    seen.len()
}
