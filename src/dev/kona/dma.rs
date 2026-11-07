//! The pipe's DMA and display lists: framed and consumed, not yet acted on.

use super::consts::*;
use super::front::Host;
use super::job::Backend;
use super::pipe::Core;
use crate::devlog::LogModule;

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
    }
}

impl Core {
    /// A DMA or display-list command: consumed so the stream stays in step.
    pub(super) fn dma_command(&mut self, _h: &Host, _be: &mut dyn Backend, selector: u32, _a: &[u32]) {
        crate::dlog!(LogModule::Kona, "kona: DMA command {selector:#x} consumed, not acted on");
    }

    pub(super) fn dma_invoke(&mut self, _h: &Host, _be: &mut dyn Backend, _pool: bool, _pairs: &[u32]) {
        crate::dlog!(LogModule::Kona, "kona: list invocation consumed, not acted on");
    }

    /// The kernel's placement of a display-list region sets the base the
    /// library's offsets count from.
    pub(super) fn dma_list_region(&mut self, a: &[u32]) {
        if a[0] != 1 || a[1] != 0 {
            return;
        }
        if a[3] == 0 || a[2] > LIST_MAX_WORDS * 4 / LIST_GRANULE {
            self.dma.list_base = 0;
            return;
        }
        self.dma.list_base = a[2] * LIST_GRANULE;
    }
}
