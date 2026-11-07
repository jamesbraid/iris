//! What a PCI host bridge supplies to its root bus.

use std::sync::Arc;

use crate::pci::PciDma;

/// What a root bus asks of its host bridge: the DMA each requester gets
/// and where each host IRQ goes. Nothing else.
pub trait PciHost: Send + Sync {
    /// The DMA identity of root slot `requester`.
    fn dma_for(&self, requester: u8) -> Arc<dyn PciDma>;
    /// Host IRQ `irq` is now `level`: the OR of every INTx line and board
    /// IRQ routed to it. Calls are serialised and in order per IRQ.
    fn set_irq(&self, irq: u8, level: bool);
}
