//! The pipe's DMA and display lists: state only so far.

use super::consts::LIST_DEFAULT;

pub(super) struct Dma {
    /// Where the current context's display-list region starts, in bytes.
    pub list_base: u32,
}

impl Dma {
    pub fn new() -> Self {
        Dma { list_base: 0 }
    }

    pub fn reset(&mut self) {
        self.list_base = 0;
        let _ = LIST_DEFAULT;
    }
}
