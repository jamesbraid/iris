//! The part of iris's `status` the board names. A headless engine has no status bar; the refresh it
//! reaches runs nothing.

use std::sync::Arc;

pub struct MachineStatus {
    refresh: Arc<crate::display::Refresh>,
}

impl MachineStatus {
    pub fn new(_heartbeat: Arc<std::sync::atomic::AtomicU64>, _fasttick: Arc<std::sync::atomic::AtomicU64>) -> Arc<Self> {
        Self::detached()
    }

    pub fn detached() -> Arc<Self> {
        Arc::new(MachineStatus { refresh: Arc::new(crate::display::Refresh) })
    }

    pub fn refresh(&self) -> &Arc<crate::display::Refresh> {
        &self.refresh
    }

    pub fn cycles(&self) -> crate::cpu::mips_core::CyclesPtr {
        crate::cpu::mips_core::CyclesPtr
    }
}
