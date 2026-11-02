//! Where the Indy and Indigo2 memory controller (MC) puts main memory.
//! The MC device and the CPU's memory description name these; ip22_bus.rs
//! re-exports them.

use crate::memory_map::{MemKind, MemRange, MemoryMap};

// Low memory (256MB at 0x08000000)
pub const LOMEM_BASE: u32 = 0x08000000;
pub const LOMEM_END: u32  = 0x18000000;

// High memory (256MB at 0x20000000)
pub const HIMEM_BASE: u32 = 0x20000000;
pub const HIMEM_END: u32  = 0x30000000;

// 128MB per bank; 4 banks total (0,1 in lomem; 2,3 in himem)
pub const BANK_SIZE: u32 = 0x08000000;

pub const PROM_BASE: u32 = 0x1FC00000;
pub const PROM_END: u32  = 0x1FD00000;

// Alias (512KB at 0x00000000) — mirrors the first 512KB of lomem (0x08000000..0x0807ffff)
// per MC spec: "The bottom 512KB of memory is just an alias for the memory located
// at address 0x08000000 to 0x0807ffff."
// An alias on the bus that adds the base of memory to the incoming address, so
// accesses go through the normal lomem decode-table entries — no direct bank pointer needed.
pub const ALIAS_BASE: u32   = 0x00000000;
pub const ALIAS_END: u32    = 0x00080000;

/// Where an IP28 can have RAM: from himem, up to four 512 MB banks.
pub const IP28_RAM_END: u32 = 0xA000_0000;

/// Where the MC can answer with memory on an Indy, an Indigo2 or an IP28.
pub fn memory_map(ip28: bool) -> MemoryMap {
    let range = |name, kind, base: u32, end: u32| MemRange::new(name, kind, base as u64, end as u64);
    let mut ranges = vec![
        range("low alias", MemKind::Alias, ALIAS_BASE, ALIAS_END),
        range("prom", MemKind::Rom, PROM_BASE, PROM_END),
    ];
    if ip28 {
        ranges.push(range("ram", MemKind::Ram, HIMEM_BASE, IP28_RAM_END));
    } else {
        ranges.push(range("lomem", MemKind::Ram, LOMEM_BASE, LOMEM_END));
        ranges.push(range("himem", MemKind::Ram, HIMEM_BASE, HIMEM_END));
    }
    MemoryMap::new(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indy_has_two_windows_a_low_alias_and_a_prom() {
        let m = memory_map(false);
        assert!(m.is_ram(0x1000), "the low alias is RAM");
        assert!(!m.contains(0x0008_0000), "the alias stops at 512 KB");
        assert!(m.is_ram(0x0800_0000));
        assert!(m.is_ram(0x17ff_ffff));
        assert!(!m.contains(0x1800_0000));
        assert_eq!(m.find(0x1fc0_0000).unwrap().kind, MemKind::Rom);
        assert!(!m.is_ram(0x1fc0_0000), "the PROM is memory but not RAM");
        assert!(m.is_ram(0x2fff_ffff));
        assert_eq!(m.end(), 0x3000_0000);
    }

    #[test]
    fn ip28_has_no_lomem() {
        let m = memory_map(true);
        assert!(!m.contains(0x0800_0000));
        assert!(m.is_ram(0x2000_0000));
        assert!(m.is_ram(0x9fff_ffff), "four 512 MB banks fit");
    }
}
