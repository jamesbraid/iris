//! Ported from include/ui/sgi-renderer.h in jamesbraid/qemu-sgi-origami.
//!
//! Only the types the scalar rasteriser and the Odyssey model still name.
//! The host (Mesa) renderer is not ported: the model takes the scalar path
//! unconditionally, so no renderer is ever created.

/// `HostRenderer`: the host renderer, never instantiated here.
#[derive(Clone, Default)]
pub struct HostRenderer {}

pub type RenderCreateResult = u32;
pub const SGI_RENDER_CREATE_OK: RenderCreateResult = 0;
pub const SGI_RENDER_CREATE_UNAVAILABLE: RenderCreateResult = 1;
pub const SGI_RENDER_CREATE_OOM: RenderCreateResult = 2;
pub const SGI_RENDER_CREATE_HOST_ERROR: RenderCreateResult = 3;

pub type RenderDrawResult = u32;
pub const SGI_RENDER_DRAW_OK: RenderDrawResult = 0;
pub const SGI_RENDER_DRAW_REFUSED: RenderDrawResult = 1;
pub const SGI_RENDER_DRAW_FAILED: RenderDrawResult = 2;
