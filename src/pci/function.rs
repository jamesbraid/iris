//! A function as the bus sees it: the trait a function implements, the
//! state the bus fills in when it attaches one, where it sits, and how a
//! bus master reaches memory.

use std::sync::Arc;

use crate::irq::IrqLine;
use super::PciConfig;

/// A PCI function, as a host bridge sees it.
///
/// BAR accesses arrive with the offset and value the device sees: whatever
/// the bridge does to byte lanes it has already done.
pub trait PciFunction: Send + Sync {
    /// The function's configuration header.
    fn config(&self) -> &PciConfig;

    /// What a configuration read of dword `index` returns to the guest. The
    /// header as stored, unless the chip decodes fewer address lines than
    /// the bus gives it (IOC3 repeats its first 32 bytes).
    fn config_dword(&self, index: usize) -> u32 {
        self.config().read(index)
    }

    /// A read of `width` bytes (1, 2 or 4) at `off` within BAR `bar`.
    fn bar_read(&self, bar: usize, off: u32, width: usize) -> u32;

    fn bar_write(&self, bar: usize, off: u32, val: u32, width: usize);

    /// The bridge wires the function's interrupt pin to `line`.
    fn connect_interrupt(&self, _line: IrqLine) {}

    /// The bridge gives a function that masters the bus its way to memory.
    fn connect_dma(&self, _dma: std::sync::Arc<dyn PciDma>) {}

    /// RST# reaches the function after the bus has reset its header: drop
    /// the interrupt line and return to power-on state. A function never
    /// resets its own header, so the bus's decode table cannot go stale.
    fn reset(&self) {}
}

/// Where a function sits on a bus: its device number and its function number.
/// Orders by device, then function, the order RST# visits them in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DevFn {
    pub device: u8,
    pub function: u8,
}

/// How a bus master's access ended.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciDmaStatus {
    Ok,
    /// Some of the bytes had no target: reads return zero for them and
    /// writes drop them. A bridge may also log it and raise its error
    /// interrupt.
    MasterAbort,
}

impl PciDmaStatus {
    pub fn is_ok(self) -> bool {
        self == PciDmaStatus::Ok
    }
}

/// Memory as a bus master reaches it. The bridge decides what a PCI address
/// is and in which order its bytes arrive; the function sees bytes at bus
/// addresses, as it would on the bus.
///
/// A function calls only the object `connect_dma` gave it and never learns
/// whether it is shared. The O2's MACE hands every function one object (its
/// translation is fixed); a bridge whose translation depends on the
/// mastering device, as the Octane's does, hands each its own.
pub trait PciDma: Send + Sync {
    /// `buf.len()` bytes from bus address `addr`, 64 bits wide for masters
    /// that use dual address cycles.
    fn dma_read(&self, addr: u64, buf: &mut [u8]) -> PciDmaStatus;

    /// `data` to bus address `addr`.
    fn dma_write(&self, addr: u64, data: &[u8]) -> PciDmaStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devfn_orders_by_device_then_function() {
        let a = |device, function| DevFn { device, function };
        let mut v = [a(3, 0), a(1, 1), a(1, 0)];
        v.sort();
        assert_eq!(v, [a(1, 0), a(1, 1), a(3, 0)]);
    }
}
