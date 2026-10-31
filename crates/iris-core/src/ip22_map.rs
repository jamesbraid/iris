//! Where the Indy and Indigo2 memory controller (MC) puts main memory.
//! The MC device and the CPU's developer undo buffer name these; ip22_bus.rs
//! re-exports them.

// Low memory (256MB at 0x08000000)
pub const LOMEM_BASE: u32 = 0x08000000;
pub const LOMEM_END: u32  = 0x18000000;

// High memory (256MB at 0x20000000)
pub const HIMEM_BASE: u32 = 0x20000000;
pub const HIMEM_END: u32  = 0x30000000;

// 128MB per bank; 4 banks total (0,1 in lomem; 2,3 in himem)
pub const BANK_SIZE: u32 = 0x08000000;
