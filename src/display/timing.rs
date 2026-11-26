//! The video timing a head runs on: what a timing generator is programmed to, and the periods that follow.

/// The shortest field period accepted: 1 kHz, well above any SGI display.
const MIN_PERIOD_NS: u64 = 1_000_000;

pub(super) const NS_PER_SECOND: u128 = 1_000_000_000;

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
    /// 1280x1024 at 60 Hz (1688x1066 at 107.964 MHz), what every board ran at
    /// before it decoded its own timing.
    pub const HZ60: VideoTiming =
        VideoTiming { h_total: 1688, v_total: 1066, pixel_clock_hz: 107_964_480, vblank_lines: 42, fields: 1 };

    fn frame_clocks(&self) -> u128 {
        self.h_total as u128 * self.v_total as u128
    }

    /// Nanoseconds from field 0 to field `k`.
    pub(super) fn field_offset_ns(&self, k: u64) -> u64 {
        (k as u128 * self.frame_clocks() * NS_PER_SECOND / (self.pixel_clock_hz as u128 * self.fields as u128)) as u64
    }

    pub fn field_period_ns(&self) -> u64 {
        self.field_offset_ns(1)
    }

    pub fn blank_ns(&self) -> u64 {
        (self.vblank_lines as u128 * self.h_total as u128 * NS_PER_SECOND / self.pixel_clock_hz as u128) as u64
    }

    /// Frames per second, in millihertz.
    pub fn refresh_mhz(&self) -> u64 {
        (self.pixel_clock_hz as u128 * 1000 / self.frame_clocks().max(1)) as u64
    }

    /// Whether the thread can run this timing. The tables come from the
    /// guest, so a timing is checked before anything is armed from it.
    pub fn valid(&self) -> bool {
        self.h_total != 0
            && self.v_total != 0
            && self.pixel_clock_hz != 0
            && (1..=2).contains(&self.fields)
            && self.field_period_ns() >= MIN_PERIOD_NS
            && self.field_period_ns() <= NS_PER_SECOND as u64
            && self.blank_ns() < self.field_period_ns()
    }
}
