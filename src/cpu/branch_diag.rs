//! The R10000 family's CP0 Diagnostic register (register 22) and the branch
//! prediction table it reaches.
//!
//! Software writes the prediction algorithm, and the index, operation and
//! state fields through which it reaches the table. The rest of the register
//! reports what the branch caches and the instruction TLB last did; those bits
//! are reserved here and read as zero. Every write also names a table
//! operation, since the operation field has no idle value: 0 reads the indexed
//! entry into the state field, 1 writes the state field into the entry, and 2
//! and 3 set the whole line of eight entries holding the index to zeroes or
//! ones. On hardware a read's result arrives once the table port frees up;
//! here it is done at once.
//!
//! A model that has no such register has a zero mask: reads return zero and
//! writes are ignored, as they were before this existed. The writable mask is
//! an inherited approximation, not a measurement of any processor.
//!
//! Ported from helper_mtc0_r10k_diagnostic in
//! target/mips/tcg/system/r10k_helper.c in jamesbraid/qemu-sgi-origami, with
//! the mask of its R16000 definition in target/mips/cpu-defs.c.inc. The table
//! has 512 entries; QEMU's 2048-entry R12000 table is not modelled.

/// Entries in the table: two bits each, indexed by VA[11:3] of a branch.
pub const BPT_ENTRIES: usize = 512;
/// Entries a line operation sets together.
const BPT_LINE: usize = 8;

const OP_SHIFT: u32 = 0;
const IDX_SHIFT: u32 = 3;
const IDX_BITS: u32 = 9;
const STATE_SHIFT: u32 = 14;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchDiag {
    mask: u32,
    value: u32,
    table: Vec<u8>,
}

impl BranchDiag {
    /// A register whose software-written bits are `mask`. Zero is a model
    /// without the register.
    pub fn with_mask(mask: u32) -> Self {
        let table = if mask == 0 { Vec::new() } else { vec![0; BPT_ENTRIES] };
        Self { mask, value: 0, table }
    }
    /// Whether the model has the register at all.
    pub fn is_present(&self) -> bool {
        self.mask != 0
    }
    pub fn read(&self) -> u64 {
        u64::from(self.value)
    }
    pub fn write(&mut self, value: u64) {
        if self.mask == 0 {
            return;
        }
        self.value = value as u32 & self.mask;
        let index = ((self.value >> IDX_SHIFT) & ((1 << IDX_BITS) - 1)) as usize;
        match (self.value >> OP_SHIFT) & 3 {
            0 => {
                self.value = (self.value & !(3 << STATE_SHIFT)) | (u32::from(self.table[index]) << STATE_SHIFT);
            }
            1 => self.table[index] = ((self.value >> STATE_SHIFT) & 3) as u8,
            op => {
                let line = index & !(BPT_LINE - 1);
                self.table[line..line + BPT_LINE].fill(if op == 3 { 3 } else { 0 });
            }
        }
    }
    /// Cold reset: the register and every table entry read zero.
    pub fn reset(&mut self) {
        self.value = 0;
        self.table.fill(0);
    }
    /// The table as 16 words of 32 two-bit entries, for snapshots.
    pub fn table_words(&self) -> Vec<u64> {
        self.table
            .chunks(32)
            .map(|chunk| chunk.iter().enumerate().fold(0, |word, (i, &state)| word | u64::from(state & 3) << (2 * i)))
            .collect()
    }
    /// Restore the register and table saved by [`Self::table_words`]. Ignored
    /// by a model without the register.
    pub fn restore(&mut self, value: u32, words: &[u64]) {
        if self.mask == 0 {
            return;
        }
        self.value = value & self.mask;
        for (entry, state) in self.table.iter_mut().enumerate() {
            *state = words.get(entry / 32).map_or(0, |word| (word >> (2 * (entry % 32)) & 3) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    // Cases follow tests/functional/mips64/roms/diagregs.c in
    // jamesbraid/qemu-sgi-origami, which runs them against its R16000: the
    // numbers in the assertion messages are the ROM's. The ROM's other checks
    // (FrameMask, the performance counters) need a guest.
    use super::*;

    const R16000_MASK: u32 = 0x0403_cffb;
    const BPMODE_MASK: u64 = 3 << 16;
    const BPSTATE_MASK: u64 = 3 << 14;
    const BIT26: u64 = 1 << 26;
    /// The ROM's DIAG_RESERVED with bit 26 removed, as it does for the R16000.
    const RESERVED: u64 = 0xfff0_ffff_0000_0000 | (0x1f << 23) & !BIT26 | (3 << 12) | (1 << 2);
    const READ: u64 = 0;
    const WRITE: u64 = 1;
    const ZEROES: u64 = 2;
    const ONES: u64 = 3;
    fn idx(i: u64) -> u64 { i << IDX_SHIFT }
    fn state(s: u64) -> u64 { s << STATE_SHIFT }

    /// A model without the register keeps the old behaviour: zero, whatever is written.
    #[test]
    fn a_model_without_the_register_reads_zero_and_ignores_writes() {
        let mut diag = BranchDiag::with_mask(0);
        diag.write(u64::MAX);
        assert_eq!(diag.read(), 0);
        diag.restore(u32::MAX, &[u64::MAX; 16]);
        assert_eq!(diag.read(), 0);
    }

    /// The reset value, a cold table read at both ends, bit 26 and the mode
    /// surviving each other, and the writable mask.
    #[test]
    fn reset_value_reserved_bits_and_mode_round_trip() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        assert_eq!(diag.read(), 0, "33");
        diag.write(idx(0) | READ);
        assert_eq!(diag.read() & BPSTATE_MASK, 0, "34");
        diag.write(idx(511) | READ);
        assert_eq!(diag.read() & BPSTATE_MASK, 0, "35");
        assert_eq!(diag.read() & RESERVED, 0, "4");
        assert_eq!(diag.read() & BPMODE_MASK, 0, "5");

        let before = diag.read();
        diag.write(before | BIT26);
        assert_eq!(diag.read() & BIT26, BIT26, "29");
        assert_eq!(diag.read() & BPMODE_MASK, before & BPMODE_MASK, "30");
        assert_eq!(diag.read() & !BIT26, before & !BIT26, "36");
        diag.write(before);
        assert_eq!(diag.read() & BIT26, before & BIT26, "31");
        assert_eq!(diag.read() & !BIT26, before & !BIT26, "37");

        for mode in 0..4u64 {
            diag.write(mode << 16);
            assert_eq!(diag.read() & BPMODE_MASK, mode << 16, "6 to 9: mode {mode}");
        }
        diag.write(u64::MAX);
        assert_eq!(diag.read() & RESERVED, 0, "10");
        assert_eq!(diag.read() & BPMODE_MASK, BPMODE_MASK, "11");
        assert_eq!(diag.read(), 0x0403_cffb, "38");
        diag.write(before);
        assert_eq!(diag.read() & BPMODE_MASK, before & BPMODE_MASK, "39");
        diag.write(0);
        assert_eq!(diag.read() & BPMODE_MASK, 0, "12");
        assert_eq!(diag.read() & RESERVED, 0, "13");
    }

    /// Table write, read, line operations and the last line.
    #[test]
    fn table_operations_read_write_and_fill_lines() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        diag.write(state(2) | idx(5) | WRITE);
        assert_eq!(diag.read(), state(2) | idx(5) | WRITE, "14: a write keeps every field");
        diag.write(state(1) | idx(6) | WRITE);
        // The state written with a read is overwritten by the entry.
        diag.write(state(3) | idx(5) | READ);
        assert_eq!(diag.read(), state(2) | idx(5), "15");
        diag.write(idx(6) | READ);
        assert_eq!(diag.read(), state(1) | idx(6), "16");

        // A mid-line index initializes the whole line; entry 8 is the next line.
        diag.write(state(1) | idx(8) | WRITE);
        diag.write(idx(4) | ONES);
        diag.write(idx(5) | READ);
        assert_eq!(diag.read(), state(3) | idx(5), "17");
        diag.write(idx(0) | READ);
        assert_eq!(diag.read(), state(3) | idx(0), "18");
        diag.write(idx(8) | READ);
        assert_eq!(diag.read(), state(1) | idx(8), "19");
        diag.write(idx(7) | ZEROES);
        diag.write(idx(5) | READ);
        assert_eq!(diag.read(), idx(5), "20");

        diag.write(idx(511) | ONES);
        diag.write(idx(511) | READ);
        assert_eq!(diag.read(), state(3) | idx(511), "21");
        diag.write(idx(504) | READ);
        assert_eq!(diag.read(), state(3) | idx(504), "22");
        diag.write(idx(503) | READ);
        assert_eq!(diag.read(), idx(503), "23");
        diag.write(0);
        assert_eq!(diag.read(), 0, "24");
    }

    /// A cold reset clears the register and the table; a save and restore
    /// brings both back.
    #[test]
    fn reset_clears_and_a_snapshot_restores_register_and_table() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        diag.write(idx(77) | state(3) | WRITE);
        diag.write(idx(77) | (1 << 16));
        let (value, words) = (diag.read() as u32, diag.table_words());
        assert_eq!(words.len(), 16);
        let mut copy = BranchDiag::with_mask(R16000_MASK);
        copy.restore(value, &words);
        assert_eq!(copy, diag);
        diag.reset();
        assert_eq!(diag.read(), 0);
        diag.write(idx(77));
        assert_eq!(diag.read() & BPSTATE_MASK, 0);
    }
}
