//! The pieces of the CPU that devices name: the ISA constants and
//! disassembler, the cycle pointer, SMP bookkeeping and JIT feedback. The
//! CPU itself is the iris-cpu crate, which re-exports these as `cpu::*`.

pub mod mips_isa;
pub mod mips_dis;
pub mod cycles;
pub mod smp;
pub mod jit_feedback;
