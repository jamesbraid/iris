//! Ported from include/ui/sgi-renderer.h in jamesbraid/qemu-sgi-origami.
//!
//! Only the types the scalar rasteriser and the Odyssey model still name.
//! The host (Mesa) renderer is not ported: the model takes the scalar path
//! unconditionally, so no renderer is ever created.

/// `SGIRenderer`: the host renderer, never instantiated here.
#[derive(Clone, Default)]
pub struct SGIRenderer {}

pub type SGIRenderCreateResult = u32;
pub const SGI_RENDER_CREATE_OK: SGIRenderCreateResult = 0;
pub const SGI_RENDER_CREATE_UNAVAILABLE: SGIRenderCreateResult = 1;
pub const SGI_RENDER_CREATE_OOM: SGIRenderCreateResult = 2;
pub const SGI_RENDER_CREATE_HOST_ERROR: SGIRenderCreateResult = 3;

pub type SGIRenderDrawResult = u32;
pub const SGI_RENDER_DRAW_OK: SGIRenderDrawResult = 0;
pub const SGI_RENDER_DRAW_REFUSED: SGIRenderDrawResult = 1;
pub const SGI_RENDER_DRAW_FAILED: SGIRenderDrawResult = 2;
