//! What the widgets on the Octane's Crossbow share: how they raise
//! interrupts at HEART and how, as bus masters, they reach memory.

/// HEART as an interrupt target. A widget's interrupt is a write of a
/// vector (the HEART interrupt bit, 0 to 63) to HEART's widget space;
/// `set` false is the matching clear.
pub trait XioInterruptSink: Send + Sync {
    fn xio_interrupt(&self, vector: u8, set: bool);
}

/// System memory as an XIO bus master reaches it through HEART, at
/// physical byte addresses. False if any byte is not memory: those read
/// as zero and writes to them go nowhere.
pub trait XioMemory: Send + Sync {
    fn read(&self, phys: u64, buf: &mut [u8]) -> bool;
    fn write(&self, phys: u64, data: &[u8]) -> bool;
}

/// Widget numbers on the Octane's Crossbow.
pub const WIDGET_XBOW: u8 = 0x0;
pub const WIDGET_HEART: u8 = 0x8;
pub const WIDGET_BRIDGE: u8 = 0xf;
/// The four ports graphics and expansion boards take.
pub const WIDGET_GFX_FIRST: u8 = 0x9;

/// A widget's 16 MB small window in the processor's physical space.
pub const fn widget_small_base(widget: u8) -> u32 {
    0x1000_0000 | ((widget as u32) << 24)
}
pub const WIDGET_SMALL_SIZE: u32 = 0x0100_0000;
