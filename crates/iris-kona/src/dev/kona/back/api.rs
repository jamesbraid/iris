//! What a front end calls: the KONA raster back end over its own pixel
//! store, with no host interface, command decoder or display behind it.
//!
//! The front end decodes drawing into window-space triangles and clears
//! (`WVertex`, `ClearReq`) and the draw state they were issued under
//! (`DrawState`). The back end sets them up and runs them on the CPU tile
//! scheduler or through the GPU store, into planes of 32-bit words.
//!
//! `submit` is the body of `KonaBackend::flush` for the triangle and clear
//! paths, without the board's counters.

use std::sync::Arc;

use super::raster::setup::{self, Shape};
use super::raster::{ClearReq, DrawState, WVertex};
use super::store::texture::TextureStore;
use super::store::{Store, StoreOp, DID_PLANE};
use super::tiles::{ClearJob, RasterJob, Tiles};
use crate::wgpu::Gpu;

const RING: usize = 1 << 10;
const BUDGET: u64 = 1 << 17;

/// One triangle, three window-space vertices.
pub type Tri = [WVertex; 3];

/// The raster back end and the store it draws into.
pub struct KonaBack {
    store: Arc<Store>,
    tiles: Option<Tiles>,
}

impl KonaBack {
    /// A back end over a `width` x `height` store with `texture_bytes` of
    /// texture memory. On the CPU, `threads` tile workers (0 runs the jobs
    /// on the caller's thread) over `tile`-pixel tiles; on the GPU when one
    /// is given.
    pub fn new(width: u32, height: u32, texture_bytes: u64, tile: u32, threads: usize, gpu: Option<&Arc<Gpu>>) -> Result<KonaBack, String> {
        let tex = TextureStore::with_capacity_bytes(texture_bytes);
        let (store, tiles) = match gpu {
            Some(g) => (Store::with_gpu(g.clone(), width, height, tex)?, None),
            None => {
                let store = Arc::new(Store::with_texture(width, height, tex));
                let tiles = Tiles::new(store.clone(), tile, threads, RING, BUDGET, None).map_err(|e| format!("tile scheduler: {e:?}"))?;
                return Ok(KonaBack { store, tiles: Some(tiles) });
            }
        };
        Ok(KonaBack { store: Arc::new(store), tiles })
    }

    /// Whether the GPU store draws.
    pub fn is_gpu(&self) -> bool {
        self.store.is_gpu()
    }

    /// Draw `tris` under `ds`.
    pub fn draw(&mut self, ds: &DrawState, tris: &[Tri]) {
        let drawable = !ds.clip.is_empty() && (ds.colours != 0 || (ds.depth_test && ds.depth_bits != 0) || (ds.stencil.enabled && ds.stencil_bits != 0));
        if !drawable {
            return;
        }
        self.arrays(ds);
        let gpu = self.store.is_gpu();
        let state = match (&mut self.tiles, gpu) {
            (Some(t), false) => t.state(*ds),
            _ => 0,
        };
        for t in tris {
            // A triangle with no area or no sample, or off the screen, draws nothing.
            let Ok(job) = setup::shaped(t, ds, state, Shape::Plain) else { continue };
            if gpu {
                self.store.apply(StoreOp::Tri { ds, job: &job });
            } else if let Some(tiles) = &mut self.tiles {
                let _ = tiles.submit(RasterJob::Tri(job));
            }
        }
    }

    /// Clear `req`'s rectangle under `ds`: the words it names, for the
    /// buffers the state draws to.
    pub fn clear(&mut self, ds: &DrawState, req: &ClearReq) {
        let mut req = *req;
        req.colour = req.colour.filter(|_| ds.colours != 0);
        req.depth = req.depth.filter(|_| ds.depth_bits != 0);
        req.stencil = req.stencil.filter(|_| ds.stencil_bits != 0);
        if req.rect.is_empty() || (req.colour.is_none() && req.depth.is_none() && req.stencil.is_none()) {
            return;
        }
        self.arrays(ds);
        if self.store.is_gpu() {
            self.store.apply(StoreOp::Clear { ds, req: &req });
        } else if let Some(tiles) = &mut self.tiles {
            let state = tiles.state(*ds);
            let _ = tiles.submit(RasterJob::Clear(ClearJob { req, state }));
        }
    }

    /// Wait for every job, so the planes can be read.
    pub fn drain(&mut self) -> Result<(), String> {
        match &mut self.tiles {
            Some(t) => t.drain().map_err(|e| format!("tile scheduler: {e:?}")),
            None => {
                self.store.observe_all();
                Ok(())
            }
        }
    }

    /// Word `w` of the pixel at (x, y). Call `drain` first.
    pub fn read(&self, x: u32, y: u32, w: usize) -> u32 {
        self.store.cpu().read(x, y, w)
    }

    /// A hash of every plane the draws allocated, the form iris's golden
    /// scenes pin. Call `drain` first.
    pub fn checksum(&self) -> u64 {
        self.store.cpu().checksum()
    }

    /// Allocate the arrays a job writes, before a worker can see it.
    fn arrays(&self, ds: &DrawState) {
        let cpu = self.store.cpu();
        ds.colour[..ds.colours as usize].iter().for_each(|&w| cpu.ensure(w as usize));
        if ds.depth_bits != 0 {
            cpu.ensure(ds.depth_word as usize);
        }
        if ds.stencil_bits != 0 {
            cpu.ensure(ds.stencil_word as usize);
        }
        if ds.ownership.is_some() {
            cpu.ensure(DID_PLANE);
        }
        if let Some(ms) = &ds.samples {
            [ms.colour, ms.depth, ms.stencil].iter().for_each(|&(first, n)| (first..first + n).for_each(|w| cpu.ensure(w as usize)));
        }
    }
}
