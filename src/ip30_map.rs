//! How the Octane's processors address memory and Crosstalk widgets through
//! HEART: where SDRAM and the boot flash sit, the widget numbers and the small
//! and large windows. The graphics boards name these to find their own
//! registers, and the CPU's tests build the memory description; ip30.rs
//! re-exports them.

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

/// Where SDRAM space starts. HEART's bank descriptors count from here in
/// 32 MB units.
pub const RAM_BASE: u64 = 0x2000_0000;
/// Low memory at physical 0: the first `LOW_SIZE` bytes of SDRAM, which the
/// processor reaches only there.
pub const LOW_SIZE: u64 = 0x4000;
/// The boot flash window, both chip selects, as the processor addresses it.
pub const BOOT_FLASH: std::ops::Range<u64> = 0x1fc0_0000..0x2000_0000;

/// Where an Octane can answer with memory, given `banks` in MB. The PROM
/// chooses where each bank goes and may leave a hole to align one, so RAM is
/// taken to reach as far as all the populated banks would if each had a slot
/// the size of the largest.
pub fn memory_map(banks_mb: &[u32]) -> crate::memory_map::MemoryMap {
    use crate::memory_map::{MemKind, MemRange, MemoryMap};
    let populated = banks_mb.iter().filter(|&&mb| mb != 0).count() as u64;
    let largest = banks_mb.iter().copied().max().unwrap_or(0) as u64;
    MemoryMap::new(vec![
        MemRange::new("low memory", MemKind::Alias, 0, LOW_SIZE),
        MemRange::new("flash", MemKind::Rom, BOOT_FLASH.start, BOOT_FLASH.end),
        MemRange::new("ram", MemKind::Ram, RAM_BASE, RAM_BASE + (populated * largest << 20)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn octane_ram_follows_the_configured_banks() {
        let full = memory_map(&[2048, 2048, 2048, 2048]);
        assert!(full.is_ram(0x2000_0000));
        assert!(full.is_ram(0x2_1fff_ffff), "the last byte of 8 GB");
        assert!(!full.contains(0x2_2000_0000));
        assert_eq!(full.end(), 0x2_2000_0000);

        let small = memory_map(&[64, 0, 0, 0]);
        assert_eq!(small.end(), 0x2000_0000 + 64 * MB);
        assert!(small.is_ram(0x100), "low memory");
        assert!(!small.contains(0x4000), "low memory is 16 KB");
    }

    #[test]
    fn octane_range_holds_mixed_banks_however_the_prom_aligns_them() {
        let m = memory_map(&[64, 2048, 0, 0]);
        assert_eq!(m.end(), 0x2000_0000 + 4096 * MB, "64 MB, a hole to 2 GB, then 2 GB");
    }
}
