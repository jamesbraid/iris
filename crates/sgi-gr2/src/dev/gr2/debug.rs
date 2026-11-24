//! The engine's stand-in for iris's `debug.rs`: no annotated trace, and one place a test can make a worker
//! thread panic (`Point::Exec`), since every queued entry asks whether it is traced.

use super::Gr2;

pub const TRACE_HQ: u32 = 1;
pub const TRACE_RE3: u32 = 2;
pub const TRACE_CPU: u32 = 4;

pub struct Gr2Trace;

impl Gr2Trace {
    pub const fn new() -> Self {
        Gr2Trace
    }
}

pub(super) fn pll_describe(_b: &[u8; 8]) -> String {
    String::new()
}

impl Gr2 {
    #[inline(always)]
    pub(super) fn tracing(&self, _what: u32) -> bool {
        #[cfg(feature = "fault-injection")]
        sgi_engine::inject::hit(sgi_engine::inject::Point::Exec);
        false
    }

    pub(super) fn trace_cpu(&self, _write: bool, _off: u32, _val: u32) {}
    pub(super) fn trace_flush(&self, _force: bool) {}
    pub(super) fn trace_note(&self, _text: &str) {}
    pub(super) fn trace_hq_entry(&self, _index: u32, _val: u32) {}
    pub(super) fn trace_hq_exec(&self, _desc: &str) {}
    pub(super) fn trace_re3(&self, _addr: u32, _val: u64, _regs: &[u32; 64]) {}
}
