//! What a PCI host bridge does to a CPU access on its way to a function.
//!
//! The bus (`pci/`) has no byte order. A host chooses how bytes move per
//! window as a [`Lanes`] value, and the physical bus maps [`PciBar`]
//! entries that apply it, so the processor reaches a function with no
//! search of the bus and no host lock. Only hosts import this module.

use std::sync::Arc;

use crate::pci::PciFunction;
use crate::traits::{BusDevice, BusRead16, BusRead32, BusRead64, BusRead8, BUS_OK};

/// How a CPU access becomes bus cycles. Functions are little-endian and see
/// offsets and values as they would on the bus; the CPU is big-endian.
/// SGI's names for the two modes are byte stream and word values.
///
/// `n` is the bytes in one cycle; the CPU's value is the number it loaded
/// or stores, so a little-endian function reading it back sees the same
/// number except where the table says the bytes are reversed.
///
/// | Variant | Size | Address | Cycles | Placement |
/// |---|---|---|---|---|
/// | `ByteStream` | 1, 2, 4 | as given | one | PCI byte i is the value's i-th byte in memory order |
/// | `ByteStream` | 8 | as given | two, lower address first | the same, a word each |
/// | `WordValues{4}` | 1, 2 | `off ^ (4 - size)` | one | the value |
/// | `WordValues{4}` | 4 | as given | one | the value |
/// | `WordValues{4}` | 8 | as given | two, lower address first | the high word, then the low |
/// | `WordValues{8}` | 1, 2, 4 | `off ^ (8 - size)` | one | the value |
/// | `WordValues{8}` | 8 | as given | one (two word calls) | the low word, then the high |
///
/// MACE is `WordValues{4}`: bytes `^3`, halves `^2`, words unchanged
/// (O2 Device Driver Programming Guide 007-3443-002, pp. 397-398, Table
/// 15-3, established). Its 64-bit PIO is not documented there; the two-word
/// split above is iris's existing behaviour and is unknown. The Bridge picks
/// per window from Device(x): its programmed swap bit gives `ByteStream`,
/// and its 64-bit device bit gives the width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lanes {
    ByteStream,
    WordValues { width: u8 },
}

impl Lanes {
    /// Word values for a bus `width` bytes wide (4 or 8). Anything else is a
    /// machine bug.
    pub fn word_values(width: u8) -> Self {
        assert!(matches!(width, 4 | 8), "pci: no word-value lanes for a {width}-byte bus");
        Lanes::WordValues { width }
    }

    /// A CPU read of `size` bytes (1, 2, 4 or 8) whose first byte is at BAR
    /// offset `off`.
    pub fn read(self, f: &dyn PciFunction, bar: usize, off: u32, size: usize) -> u64 {
        match self {
            Lanes::ByteStream => match size {
                1 => f.bar_read(bar, off, 1) as u64,
                2 => (f.bar_read(bar, off, 2) as u16).swap_bytes() as u64,
                4 => f.bar_read(bar, off, 4).swap_bytes() as u64,
                _ => {
                    let first = f.bar_read(bar, off, 4).swap_bytes() as u64;
                    first << 32 | f.bar_read(bar, off.wrapping_add(4), 4).swap_bytes() as u64
                }
            },
            Lanes::WordValues { width } => {
                let width = width as usize;
                if size == 8 {
                    let (a, b) = (f.bar_read(bar, off, 4) as u64, f.bar_read(bar, off.wrapping_add(4), 4) as u64);
                    return if width == 8 { b << 32 | a } else { a << 32 | b };
                }
                f.bar_read(bar, off ^ (width - size) as u32, size) as u64
            }
        }
    }

    pub fn write(self, f: &dyn PciFunction, bar: usize, off: u32, size: usize, val: u64) {
        match self {
            Lanes::ByteStream => match size {
                1 => f.bar_write(bar, off, val as u32, 1),
                2 => f.bar_write(bar, off, (val as u16).swap_bytes() as u32, 2),
                4 => f.bar_write(bar, off, (val as u32).swap_bytes(), 4),
                _ => {
                    f.bar_write(bar, off, ((val >> 32) as u32).swap_bytes(), 4);
                    f.bar_write(bar, off.wrapping_add(4), (val as u32).swap_bytes(), 4);
                }
            },
            Lanes::WordValues { width } => {
                let width = width as usize;
                if size == 8 {
                    let (hi, lo) = ((val >> 32) as u32, val as u32);
                    let (a, b) = if width == 8 { (lo, hi) } else { (hi, lo) };
                    f.bar_write(bar, off, a, 4);
                    f.bar_write(bar, off.wrapping_add(4), b, 4);
                    return;
                }
                f.bar_write(bar, off ^ (width - size) as u32, val as u32, size);
            }
        }
    }
}

/// A run of a host's CPU window that one BAR decodes whole, as a bus device
/// the physical bus maps over the host's own window. `lanes` is by value:
/// the access is one `match` and the function's own call.
pub struct PciBar {
    f: Arc<dyn PciFunction>,
    bar: usize,
    /// The CPU address bits the host decodes, and where in them the BAR's
    /// byte 0 sits (it may lie below the window: a BAR larger than it).
    mask: u64,
    base: u64,
    lanes: Lanes,
}

impl PciBar {
    pub fn new(f: Arc<dyn PciFunction>, bar: usize, base: u64, mask: u64, lanes: Lanes) -> Self {
        Self { f, bar, mask, base, lanes }
    }

    #[inline]
    fn off(&self, addr: u64) -> u32 {
        (addr & self.mask).wrapping_sub(self.base) as u32
    }
}

impl BusDevice for PciBar {
    fn read8(&self, a: u64) -> BusRead8 { BusRead8::ok(self.lanes.read(&*self.f, self.bar, self.off(a), 1) as u8) }
    fn read16(&self, a: u64) -> BusRead16 { BusRead16::ok(self.lanes.read(&*self.f, self.bar, self.off(a), 2) as u16) }
    fn read32(&self, a: u64) -> BusRead32 { BusRead32::ok(self.lanes.read(&*self.f, self.bar, self.off(a), 4) as u32) }
    fn read64(&self, a: u64) -> BusRead64 { BusRead64::ok(self.lanes.read(&*self.f, self.bar, self.off(a), 8)) }
    fn write8(&self, a: u64, v: u8) -> u32 { self.lanes.write(&*self.f, self.bar, self.off(a), 1, v as u64); BUS_OK }
    fn write16(&self, a: u64, v: u16) -> u32 { self.lanes.write(&*self.f, self.bar, self.off(a), 2, v as u64); BUS_OK }
    fn write32(&self, a: u64, v: u32) -> u32 { self.lanes.write(&*self.f, self.bar, self.off(a), 4, v as u64); BUS_OK }
    fn write64(&self, a: u64, v: u64) -> u32 { self.lanes.write(&*self.f, self.bar, self.off(a), 8, v); BUS_OK }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pci::testing::TestCard;
    use crate::pci::Bar;

    const V: u64 = 0x0102_0304_0506_0708;

    fn card() -> Arc<TestCard> {
        TestCard::new([Bar::Mem(0x1000), Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused, Bar::Unused], 0)
    }

    /// The register file after a write of V's top `size` bytes at `off`, and
    /// the BAR calls it took.
    fn written(lanes: Lanes, off: u32, size: usize) -> (Vec<u8>, Vec<(u32, usize)>) {
        let c = card();
        lanes.write(&*c, 0, off, size, V >> (64 - 8 * size));
        let bytes = c.mem()[..16].to_vec();
        (bytes, c.calls())
    }

    fn at(bytes: &[u8], off: usize, n: usize) -> Vec<u8> {
        bytes[off..off + n].to_vec()
    }

    #[test]
    fn every_row_of_the_lane_table() {
        let be = |n: usize| V.to_be_bytes()[..n].to_vec();
        let le = |n: usize| (V >> (64 - 8 * n)).to_le_bytes()[..n].to_vec();
        // The value stored is V's top `size` bytes: 0x01, 0x0102, ...
        for off in 0..8u32 {
            let o = off as usize;
            // ByteStream: address as given, bytes in memory order.
            for size in [1usize, 2, 4, 8] {
                if off as usize % size != 0 { continue }
                let (b, calls) = written(Lanes::ByteStream, off, size);
                assert_eq!(at(&b, o, size), be(size), "byte stream size {size} off {off}");
                let want: Vec<_> = if size == 8 { vec![(off, 4), (off + 4, 4)] } else { vec![(off, size)] };
                assert_eq!(calls, want, "cycles, byte stream size {size}");
            }
            // WordValues{4}: sub-word accesses move to off ^ (4 - size), values unchanged.
            for size in [1usize, 2, 4] {
                if off as usize % size != 0 { continue }
                let (b, calls) = written(Lanes::word_values(4), off, size);
                let a = (off ^ (4 - size) as u32) as usize;
                assert_eq!(at(&b, a, size), le(size), "word values 4 size {size} off {off}");
                assert_eq!(calls, vec![(a as u32, size)]);
            }
            if off % 8 == 0 {
                let (b, calls) = written(Lanes::word_values(4), off, 8);
                assert_eq!(at(&b, o, 4), ((V >> 32) as u32).to_le_bytes(), "high word first");
                assert_eq!(at(&b, o + 4, 4), (V as u32).to_le_bytes());
                assert_eq!(calls, vec![(off, 4), (off + 4, 4)]);
            }
            // WordValues{8}.
            for size in [1usize, 2, 4] {
                if off as usize % size != 0 { continue }
                let (b, calls) = written(Lanes::word_values(8), off, size);
                let a = (off ^ (8 - size) as u32) as usize;
                assert_eq!(at(&b, a, size), le(size), "word values 8 size {size} off {off}");
                assert_eq!(calls, vec![(a as u32, size)]);
            }
            if off == 0 {
                let (b, calls) = written(Lanes::word_values(8), 0, 8);
                assert_eq!(at(&b, 0, 8), V.to_le_bytes(), "one cycle of eight reversed bytes");
                assert_eq!(calls, vec![(0, 4), (4, 4)], "carried as two word calls, low first");
            }
        }
    }

    /// Whatever a write leaves, the same access reads back.
    #[test]
    fn a_read_returns_what_the_same_write_stored() {
        for lanes in [Lanes::ByteStream, Lanes::word_values(4), Lanes::word_values(8)] {
            for size in [1usize, 2, 4, 8] {
                for off in (0..16u32).step_by(size) {
                    let c = card();
                    let v = if size == 8 { V } else { V >> (64 - 8 * size) };
                    lanes.write(&*c, 0, off, size, v);
                    assert_eq!(lanes.read(&*c, 0, off, size), v, "{lanes:?} size {size} off {off}");
                }
            }
        }
    }

    /// MACE's old swizzle: bytes `^3`, halves `^2`, words as given.
    #[test]
    fn word_values_4_matches_mace_lane_swizzle() {
        for size in [1usize, 2, 4] {
            for off in 0..32u32 {
                let c = card();
                Lanes::word_values(4).read(&*c, 0, off, size);
                let want = crate::dev::mace_pci::MacePci::lane_swizzle(off, size);
                assert_eq!(c.calls(), vec![(want, size)], "size {size} off {off}");
            }
        }
    }

    /// The Bridge's old bar_cycle for a CPU access at `off`, copied here as
    /// the reference: bytes in CPU order, swap programmed or not.
    fn old_bridge_write(width: usize, swap: bool, off: u32, size: usize, val: u64, f: &dyn PciFunction) {
        let mut host = [0u8; 8];
        for (k, b) in host.iter_mut().enumerate().take(size) {
            *b = (val >> (8 * (size - 1 - k))) as u8;
        }
        let cycles: Vec<(u32, usize)> = if size > width { vec![(off, 4), (off + 4, 4)] } else { vec![(off, size)] };
        for (o, n) in cycles {
            let start = if o == off { 0 } else { 4 };
            let h = &host[start..start + n];
            let pci: Vec<u8> = (0..n).map(|i| h[if swap { i } else { n - 1 - i }]).collect();
            let boff = if swap { o } else { o ^ (width - n) as u32 };
            let mut done = 0;
            while done < n {
                let chunk = (n - done).min(4);
                let v = pci[done..done + chunk].iter().rev().fold(0u32, |a, &b| a << 8 | b as u32);
                f.bar_write(0, boff + done as u32, v, chunk);
                done += chunk;
            }
        }
    }

    #[test]
    fn word_values_matches_the_bridge_bar_cycle() {
        for (lanes, width, swap) in [(Lanes::ByteStream, 4, true), (Lanes::ByteStream, 8, true), (Lanes::word_values(4), 4, false), (Lanes::word_values(8), 8, false)] {
            for size in [1usize, 2, 4, 8] {
                for off in (0..16u32).step_by(size) {
                    let (old, new) = (card(), card());
                    let v = if size == 8 { V } else { V >> (64 - 8 * size) };
                    old_bridge_write(width, swap, off, size, v, &*old);
                    lanes.write(&*new, 0, off, size, v);
                    assert_eq!(old.mem(), new.mem(), "{lanes:?} width {width} size {size} off {off}");
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "no word-value lanes")]
    fn word_values_3_panics() {
        Lanes::word_values(3);
    }

    /// MACE's 64-bit PIO is not documented (the guide's table stops at 32
    /// bits); the two-word split, high word first, is iris's own choice and
    /// this test pins it so a change is deliberate.
    #[test]
    fn mace_64_bit_is_the_two_word_split_and_marked_unknown() {
        let c = card();
        Lanes::word_values(4).write(&*c, 0, 0x10, 8, 0x1122_3344_5566_7788);
        assert_eq!(c.mem()[0x10..0x18], [0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]);
    }

    #[test]
    fn a_pci_bar_applies_its_lanes_at_the_offset_from_its_base() {
        let c = card();
        let bar = PciBar::new(c.clone(), 0, 0x1a00_1000, 0xffff_ffff, Lanes::word_values(4));
        bar.write8(0x1a00_1084, 0xaa);
        assert_eq!(c.calls(), vec![(0x87, 1)], "off 0x84, byte lane ^3");
        assert_eq!(bar.read32(0x1a00_1084).data, 0xaa00_0000, "the byte sits in the top lane of its word");
        let hi = PciBar::new(c, 0, 0x1000, 0x01ff_ffff, Lanes::ByteStream);
        hi.write16(0x9a00_1010, 0x1234);
        assert_eq!(hi.read16(0x1a00_1010).data, 0x1234, "the host mask folds the aliases");
    }
}
