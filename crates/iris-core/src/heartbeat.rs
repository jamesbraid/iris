//! Bits of the status-bar heartbeat word.
//!
//! Devices set these on the one `AtomicU64` a board's display hands them, and
//! the status bar (crates/iris-gfx/src/disp.rs) reads and clears them once a frame. They used
//! to live on `Rex3`, which made every Ethernet and SCSI device name the
//! Newport.

pub const HB_ENET_TX:   u64 = 1 << 0;
pub const HB_ENET_RX:   u64 = 1 << 1;
pub const HB_LED_RED:   u64 = 1 << 2; // IOC front-panel red LED (persistent)
pub const HB_LED_GREEN: u64 = 1 << 3; // IOC front-panel green LED (persistent)
pub const HB_SCSI_BASE: u32 = 8; // bits 8-13 = SCSI IDs 0-5

/// Mask of persistent bits that are NOT cleared by the per-frame fetch_and.
pub const HB_PERSISTENT: u64 = HB_LED_RED | HB_LED_GREEN;
