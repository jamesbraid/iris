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
//! target/mips/tcg/system/r10k_helper.c in jamesbraid/qemu-sgi-origami.

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
    // jamesbraid/qemu-sgi-origami: the R16000 Diagnostic register's reset
    // value, reserved bits, prediction-mode round trip, bit 26 and the
    // prediction table operations.
    use super::*;

    const R16000_MASK: u32 = 0x0403_cffb;
    const BPMODE: u64 = 3 << 16;
    const BIT26: u64 = 1 << 26;
    fn index(i: u64) -> u64 { i << IDX_SHIFT }
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

    /// The register is zero after a cold reset, and its table reads zero at
    /// both ends. Reserved bits, including every bit above 32, read zero
    /// after any write; the mode field, bit 26 and the other software-written
    /// fields round-trip.
    #[test]
    fn reset_value_reserved_bits_and_mode_round_trip() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        assert_eq!(diag.read(), 0);
        for i in [0, 511] {
            diag.write(index(i));
            assert_eq!(diag.read() & state(3), 0, "entry {i} after reset");
        }
        diag.write(u64::MAX);
        assert_eq!(diag.read() & !u64::from(R16000_MASK), 0, "reserved bits stay zero");
        assert_eq!(diag.read() & (1 << 2), 0);
        for mode in 0..4u64 {
            diag.write(mode << 16);
            assert_eq!(diag.read() & BPMODE, mode << 16);
        }
        // Bit 26 can be set and cleared without disturbing the rest.
        diag.write(2 << 16);
        let before = diag.read();
        diag.write(before | BIT26);
        assert_eq!(diag.read() & BIT26, BIT26);
        assert_eq!(diag.read() & !BIT26, before & !BIT26);
        diag.write(before);
        assert_eq!(diag.read(), before);
    }

    /// Table operations: write an entry then read it back through the state
    /// field, and set a line of eight to zeroes or ones. The read leaves the
    /// state of the entry in the register.
    #[test]
    fn table_operations_read_write_and_fill_lines() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        diag.write(index(10) | state(2) | 1);
        diag.write(index(10));
        assert_eq!(diag.read() >> STATE_SHIFT & 3, 2);
        // Entry 11 shares the line; entry 16 does not.
        diag.write(index(11));
        assert_eq!(diag.read() >> STATE_SHIFT & 3, 0);
        diag.write(index(8) | 3);
        for i in 8..16 {
            diag.write(index(i));
            assert_eq!(diag.read() >> STATE_SHIFT & 3, 3, "entry {i}");
        }
        diag.write(index(16));
        assert_eq!(diag.read() >> STATE_SHIFT & 3, 0, "the next line is untouched");
        diag.write(index(10) | 2);
        for i in 8..16 {
            diag.write(index(i));
            assert_eq!(diag.read() >> STATE_SHIFT & 3, 0, "entry {i}");
        }
        // The top entry is reachable.
        diag.write(index(511) | state(1) | 1);
        diag.write(index(511));
        assert_eq!(diag.read() >> STATE_SHIFT & 3, 1);
    }

    /// A cold reset clears the register and the table; a save and restore
    /// brings both back.
    #[test]
    fn reset_clears_and_a_snapshot_restores_register_and_table() {
        let mut diag = BranchDiag::with_mask(R16000_MASK);
        diag.write(index(77) | state(3) | 1);
        diag.write(index(77) | (1 << 16));
        let (value, words) = (diag.read() as u32, diag.table_words());
        assert_eq!(words.len(), 16);
        let mut copy = BranchDiag::with_mask(R16000_MASK);
        copy.restore(value, &words);
        assert_eq!(copy, diag);
        diag.reset();
        assert_eq!(diag.read(), 0);
        diag.write(index(77));
        assert_eq!(diag.read() >> STATE_SHIFT & 3, 0);
    }
}
