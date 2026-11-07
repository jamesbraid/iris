//! Test doubles for the bus: a function that records what it is asked.

use std::sync::{Arc, Mutex};

use super::{Bar, PciConfig, PciFunction, PciFunctionState};

/// A function with the given BARs whose BAR 0 is a little-endian register
/// file, and which remembers the BAR calls it was given.
pub(crate) struct TestCard {
    pci: PciFunctionState,
    mem: Mutex<[u8; 0x1000]>,
    calls: Mutex<Vec<(u32, usize)>>,
}

impl TestCard {
    pub fn new(bars: [Bar; 6], pin: u8) -> Arc<Self> {
        Arc::new(Self {
            pci: PciFunctionState::new(PciConfig::new(0x1234, 0x5678, 0, 1, bars, pin)),
            mem: Mutex::new([0; 0x1000]),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// The register file.
    pub fn mem(&self) -> Vec<u8> {
        self.mem.lock().unwrap().to_vec()
    }

    /// The (offset, width) of each BAR call so far.
    pub fn calls(&self) -> Vec<(u32, usize)> {
        self.calls.lock().unwrap().clone()
    }
}

impl PciFunction for TestCard {
    fn state(&self) -> &PciFunctionState {
        &self.pci
    }

    fn bar_read(&self, _bar: usize, off: u32, width: usize) -> u32 {
        self.calls.lock().unwrap().push((off, width));
        let m = self.mem.lock().unwrap();
        m[off as usize..off as usize + width].iter().rev().fold(0, |a, &b| a << 8 | b as u32)
    }

    fn bar_write(&self, _bar: usize, off: u32, val: u32, width: usize) {
        self.calls.lock().unwrap().push((off, width));
        self.mem.lock().unwrap()[off as usize..off as usize + width].copy_from_slice(&val.to_le_bytes()[..width]);
    }

}
