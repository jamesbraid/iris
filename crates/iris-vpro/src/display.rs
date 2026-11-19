//! `src/display/marks.rs`, the part of iris's display the engine uses, and
//! the timing record the scan-out reports.

#[path = "../../../src/display/marks.rs"]
mod marks;

pub use marks::{Mark, Marks, Rect};

/// What a timing generator is programmed to: the totals, blanking included,
/// and the clock that counts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTiming {
    pub h_total: u32,
    pub v_total: u32,
    pub pixel_clock_hz: u64,
    pub vblank_lines: u32,
    pub fields: u32,
}
