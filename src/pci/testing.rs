//! Test doubles for the bus: a function that records what it is asked.

use std::sync::{Arc, Mutex};

use super::{Bar, PciConfig, PciFunction, PciFunctionState};

/// A function with the given BARs that remembers its last BAR access
/// (bar, offset, width) and the order of resets it saw.
pub(crate) struct TestCard {
    pci: PciFunctionState,
    pub last: Mutex<Option<(usize, u32, usize)>>,
}

impl TestCard {
    pub fn new(bars: [Bar; 6], pin: u8) -> Arc<Self> {
        Arc::new(Self {
            pci: PciFunctionState::new(PciConfig::new(0x1234, 0x5678, 0, 1, bars, pin)),
            last: Mutex::new(None),
        })
    }
}

impl PciFunction for TestCard {
    fn state(&self) -> &PciFunctionState {
        &self.pci
    }

    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32 {
        *self.last.lock().unwrap() = Some((bar, off, width));
        0x1234_5678
    }

    fn bar_write(&self, bar: usize, off: u32, _val: u32, width: usize) {
        *self.last.lock().unwrap() = Some((bar, off, width));
    }

}
