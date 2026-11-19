//! What the raster engines tell a display about their drawing, and the video timing a scan-out reports.

mod marks;

pub use marks::{Mark, Marks, Rect};

/// What a timing generator is programmed to: the totals, blanking included,
/// and the clock that counts them. The refresh rate follows from these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTiming {
    /// Pixel clocks per line.
    pub h_total: u32,
    /// Lines per frame, over all of its fields.
    pub v_total: u32,
    pub pixel_clock_hz: u64,
    /// Lines of vertical blanking per field.
    pub vblank_lines: u32,
    /// Fields per frame: 1, or 2 for field-sequential stereo and the
    /// two-channel formats. Each field has its own vertical blank.
    pub fields: u32,
}

impl VideoTiming {
    /// Frames per second, in millihertz.
    pub fn refresh_mhz(&self) -> u64 {
        let frame_clocks = self.h_total as u128 * self.v_total as u128;
        (self.pixel_clock_hz as u128 * 1000 / frame_clocks.max(1)) as u64
    }
}
