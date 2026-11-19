//! Host memory as the command parser's DMA sees it, without the bus behind
//! it, so the parser builds without iris's CPU-side bus traits.

/// A DMA access that reached no memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault;

/// Host memory, big-endian, byte addressed. Accesses are within one page.
pub trait HostMem {
    /// One 32-bit word, or `None` if the address is not memory.
    fn read32(&self, addr: u64) -> Option<u32>;
    /// Fill `out`; on a fault the unreadable bytes are zero.
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), Fault>;
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), Fault>;
}
