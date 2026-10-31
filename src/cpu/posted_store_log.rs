//! Rate limit for the log line of an uncached store the system refused.

use std::collections::HashMap;

/// Which refused stores have been reported. A guest probe loop writing to one
/// dead address would otherwise print a line per store, so each address
/// reports its first few stores and then the 8th, 16th, 32nd and so on, with
/// the running count so the gaps show. The table is capped; addresses that
/// arrive after it is full share one counter and say so.
#[derive(Default)]
pub struct PostedStoreLog {
    counts: HashMap<u64, u64>,
    /// Stores to addresses that did not fit in the table.
    overflow: u64,
}

impl PostedStoreLog {
    const MAX_ADDRS: usize = 4096;
    const ALWAYS: u64 = 4;

    /// Count a refused store to `phys_addr`. `Some((n, shared))` means print
    /// it: this is store number `n` to that address, or, with `shared`, number
    /// `n` among all addresses that did not fit in the table.
    pub fn note(&mut self, phys_addr: u64) -> Option<(u64, bool)> {
        let (n, shared) = if let Some(n) = self.counts.get_mut(&phys_addr) {
            *n += 1;
            (*n, false)
        } else if self.counts.len() < Self::MAX_ADDRS {
            self.counts.insert(phys_addr, 1);
            (1, false)
        } else {
            self.overflow += 1;
            (self.overflow, true)
        };
        (n <= Self::ALWAYS || n.is_power_of_two()).then_some((n, shared))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe loop logs a few lines, then sparsely; a new address starts over;
    /// the table stays bounded and, once full, shares one counter.
    #[test]
    fn the_log_is_bounded_per_address_and_in_total() {
        let mut log = PostedStoreLog::default();
        let shown = (1..=10_000).filter(|_| log.note(0x1e13_0400).is_some()).count();
        assert!((4..=24).contains(&shown), "first few and then sparse, got {shown}");
        assert!(log.note(0x1e13_0408).is_some(), "a different address logs from the start");

        for pa in 0..100_000u64 { log.note(pa << 3); }
        assert!(log.counts.len() <= PostedStoreLog::MAX_ADDRS);

        let mut log = PostedStoreLog::default();
        for pa in 0..PostedStoreLog::MAX_ADDRS as u64 { assert!(log.note(pa << 3).is_some()); }
        assert_eq!(log.note(0), Some((2, false)), "a tracked address keeps its own count");
        let big = 1u64 << 40;
        for i in 1..=4u64 { assert_eq!(log.note(big + i * 8), Some((i, true))); }
        assert_eq!(log.note(big + 0x100), None, "the 5th shared store is suppressed");
        assert_eq!(log.counts.len(), PostedStoreLog::MAX_ADDRS);
    }
}
