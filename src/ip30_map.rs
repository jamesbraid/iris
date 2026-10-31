//! How the Octane's processors address Crosstalk widgets through HEART: the
//! widget numbers and the small and large windows. The graphics boards name
//! these to find their own registers; ip30.rs re-exports them.

/// Widget numbers on the Octane's Crossbow.
pub const WIDGET_XBOW: u8 = 0x0;
pub const WIDGET_HEART: u8 = 0x8;
pub const WIDGET_BRIDGE: u8 = 0xf;

/// Widget `w`'s small window: 16 MiB at `0x1000_0000 + (w << 24)`.
pub const fn widget_small_base(w: u8) -> u64 {
    0x1000_0000 | (w as u64) << 24
}
pub const WIDGET_SMALL_SIZE: u64 = 0x0100_0000;
/// Widget `w`'s large window (widgets 8 to 15): 64 GiB at `w << 36`.
pub const fn widget_large_base(w: u8) -> u64 {
    (w as u64) << 36
}
pub const WIDGET_LARGE_SIZE: u64 = 1 << 36;
