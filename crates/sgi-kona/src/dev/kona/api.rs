//! What a front end calls when it has window-space triangles and clears and
//! no host interface, command decoder or display: the raster back end over
//! its own pixel store.
//!
//! The front end hands it triangles (`WVertex`), clears (`ClearReq`) and the
//! draw state they were issued under (`DrawState`). It sets them up and runs
//! them on the CPU tile scheduler or through the GPU store, into planes of
//! 32-bit words. The set-up and submission is `back::draw::flush`, the same
//! function the board's token layer calls.

use std::sync::Arc;

use super::back::draw::{self, RasterCounts, Sink};
use super::back::raster::{ClearReq, DrawState, Prims, WVertex};
use super::back::store::texture::TextureStore;
use super::back::store::Store;
use super::back::tiles::Tiles;
use crate::wgpu::Gpu;

const RING: usize = 1 << 10;
const BUDGET: u64 = 1 << 17;

/// One triangle, three window-space vertices.
pub type Tri = [WVertex; 3];

/// The raster back end and the store it draws into.
pub struct KonaBack {
    store: Arc<Store>,
    tiles: Option<Tiles>,
    counts: RasterCounts,
    tile: u32,
    threads: usize,
}

impl KonaBack {
    /// A back end over a `width` x `height` store with `texture_bytes` of
    /// texture memory. On the CPU, `threads` tile workers (0 runs the jobs
    /// on the caller's thread) over `tile`-pixel tiles; on the GPU when one
    /// is given.
    pub fn new(width: u32, height: u32, texture_bytes: u64, tile: u32, threads: usize, gpu: Option<&Arc<Gpu>>) -> Result<KonaBack, String> {
        let tex = TextureStore::with_capacity_bytes(texture_bytes);
        let store = match gpu {
            Some(g) => Store::with_gpu(g.clone(), width, height, tex)?,
            None => Store::with_texture(width, height, tex),
        };
        Ok(KonaBack { store: Arc::new(store), tiles: None, counts: RasterCounts::default(), tile, threads })
    }

    /// Whether the GPU store draws.
    pub fn is_gpu(&self) -> bool {
        self.store.is_gpu()
    }

    /// What set-up counted.
    pub fn counts(&self) -> &RasterCounts {
        &self.counts
    }

    /// Draw `tris` under `ds`.
    pub fn draw(&mut self, ds: &DrawState, tris: &[Tri]) {
        let mut prims = Prims { state: Some(*ds), ..Prims::default() };
        prims.tris.extend_from_slice(tris);
        self.flush(&mut prims);
    }

    /// Clear `req`'s rectangle under `ds`: the words it names, for the
    /// buffers the state draws to.
    pub fn clear(&mut self, ds: &DrawState, req: &ClearReq) {
        let mut prims = Prims { state: Some(*ds), ..Prims::default() };
        prims.clears.push(*req);
        self.flush(&mut prims);
    }

    fn flush(&mut self, prims: &mut Prims) {
        let (store, tile, threads) = (&self.store, self.tile, self.threads);
        let make = || Tiles::new(store.clone(), tile, threads, RING, BUDGET, None).expect("a power-of-two tile size");
        draw::flush(prims, &mut Sink { counts: &mut self.counts, store, tiles: &mut self.tiles, make: &make });
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
}
