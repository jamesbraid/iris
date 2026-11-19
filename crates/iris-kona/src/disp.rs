//! The part of iris's `disp` the display transport names: the status bar's
//! inputs, which a headless engine has none of, and the PNG writer.

use std::sync::Arc;
use std::time::Instant;

pub struct BarStats {
    pub now: Instant,
    pub screen: Option<(usize, usize)>,
}

pub struct StatusSource;

impl StatusSource {
    pub fn new(_heartbeat: Arc<std::sync::atomic::AtomicU64>, _fasttick: Arc<std::sync::atomic::AtomicU64>) -> Arc<Self> {
        Arc::new(StatusSource)
    }

    pub fn detached() -> Arc<Self> {
        Arc::new(StatusSource)
    }

    pub fn cycles(&self) -> crate::cpu::mips_core::CyclesPtr {
        crate::cpu::mips_core::CyclesPtr
    }

    pub fn snapshot(&self, now: Instant, _screen: usize) -> BarStats {
        BarStats { now, screen: None }
    }
}

/// Not available in the engine crate; callers get the error.
pub fn write_png_words(_path: impl AsRef<std::path::Path>, _px: &[u32], _stride: usize, _w: usize, _h: usize) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no PNG writer in the engine crate"))
}
