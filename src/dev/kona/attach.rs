//! What the board is given to attach to a machine.
//!
//! The board is a crosstalk-style widget: the machine decodes the widget's
//! window and calls `Kona::bus_read` / `Kona::bus_write` with offsets inside
//! the 16 MiB aperture. Everything the board does on its own initiative goes
//! out through the three handles below, so nothing here names the machine's
//! interrupt controller, memory controller or bridge: the machine's glue
//! implements the traits over whatever its fabric is.

use std::sync::Arc;

/// An interrupt packet: the pipe stores a vector to the address the driver
/// programmed into its interrupt destination registers, and the fabric
/// carries it to the widget that address names. The pipe only ever asserts;
/// the processor clears the pending bit itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptPacket {
    /// The destination widget number (bits 19:16 of the upper destination
    /// register).
    pub widget: u8,
    /// The 48-bit address within that widget (the upper register's low 16
    /// bits above the lower register's 32).
    pub address: u64,
    /// The eight-bit vector.
    pub vector: u8,
}

/// Where the board's interrupt packets go.
pub trait PacketSender: Send + Sync {
    /// Send `packet`. Called from the processor's thread or the board's own
    /// thread, never with a board lock held.
    fn send(&self, packet: InterruptPacket);
}

/// The board's inbound DMA: reads and writes of host memory at physical
/// addresses, as the packets the driver sends name them. The widget the
/// transfers go to is the machine's to choose; the packets name none.
pub trait HostDma: Send + Sync {
    /// Read `buf.len()` bytes at `addr`; false if the read fails.
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
    /// Write `data` at `addr`; false if the write fails.
    fn write(&self, addr: u64, data: &[u8]) -> bool;
}

/// The command FIFO has drained: all the work the board held is done.
///
/// The machine can use this to hand back the graphics flow-control credits
/// its processor interface holds for stores the board has now taken, and to
/// wake a processor the driver put to sleep waiting for the pipe to drain.
pub trait FifoLow: Send + Sync {
    /// Called from the board's thread once its FIFO and the words behind it
    /// are all run, outside any board lock.
    fn drained(&self);
}

/// The three handles the board is built with.
#[derive(Clone)]
pub struct KonaAttach {
    pub packets: Arc<dyn PacketSender>,
    pub dma: Arc<dyn HostDma>,
    pub fifo_low: Arc<dyn FifoLow>,
}
