//! The part of iris's `status` the board names. A headless engine has no status bar and no refresh thread:
//! the shell paces the retrace and composes through `BoardEngine`.

use std::sync::Arc;

pub struct MachineStatus;

impl MachineStatus {
    pub fn detached() -> Arc<Self> {
        Arc::new(MachineStatus)
    }

    /// The machine's display refresh, which in this crate runs nothing.
    pub fn refresh(&self) -> Arc<crate::display::Refresh> {
        Arc::new(crate::display::Refresh)
    }
}
