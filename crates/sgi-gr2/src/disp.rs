//! The part of iris's `disp` the board names: the status bar's inputs, of which a headless engine has none.

use std::sync::Arc;

pub struct StatusSource;

impl StatusSource {
    pub fn detached() -> Arc<Self> {
        Arc::new(StatusSource)
    }
}
