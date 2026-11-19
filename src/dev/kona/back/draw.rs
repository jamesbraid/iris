//! Set-up and submission of what the GL path made: the free functions the
//! back end's `flush` and the raster-only `KonaBack` share.

use super::raster::setup::Shape;
use super::raster::{self, Prims};
use super::store::{self, Store, StoreOp};
use super::tiles::{ClearJob, RasterJob, Tiles};
use std::sync::Arc;

/// What set-up counted, over the draws handed to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterCounts {
    pub draws: u64,
    pub triangles: u64,
    pub no_area: u64,
    /// Triangles with area that no pixel's sample falls inside.
    pub no_sample: u64,
    pub clears: u64,
    /// Triangles and clears that had no pixel to write: no colour or depth
    /// target, or nothing inside the clip rectangle and viewport.
    pub off_target: u64,
    /// Draws and clears in a multisampled window, and of those the draws with alpha-to-mask and with
    /// the sample mask on: each is a model choice the table does not settle (see `raster::multisample`).
    pub multisampled: u64,
    pub alpha_to_mask: u64,
    pub sample_mask: u64,
}

/// Allocate the arrays a job writes, before a worker can see it.
pub fn arrays(store: &Store, ds: &raster::DrawState) {
    ds.colour[..ds.colours as usize].iter().for_each(|&w| store.cpu().ensure(w as usize));
    if ds.depth_bits != 0 {
        store.cpu().ensure(ds.depth_word as usize);
    }
    if ds.stencil_bits != 0 {
        store.cpu().ensure(ds.stencil_word as usize);
    }
    if ds.ownership.is_some() {
        store.cpu().ensure(store::DID_PLANE);
    }
    if let Some(ms) = &ds.samples {
        [ms.colour, ms.depth, ms.stencil].iter().for_each(|&(first, n)| (first..first + n).for_each(|w| store.cpu().ensure(w as usize)));
    }
}

/// Where a flush sends its jobs: the store, the tile scheduler (made by `make` when the first job
/// comes) and the counters.
pub struct Sink<'a> {
    pub counts: &'a mut RasterCounts,
    pub store: &'a Arc<Store>,
    pub tiles: &'a mut Option<Tiles>,
    pub make: &'a dyn Fn() -> Tiles,
}

impl Sink<'_> {
    fn tiles(&mut self) -> &mut Tiles {
        let make = self.make;
        self.tiles.get_or_insert_with(make)
    }
}

/// Set up what the GL path made in `prims` and submit it, then empty it.
pub fn flush(prims: &mut Prims, b: &mut Sink<'_>) {
    // The GL path sets the state with every primitive it queues and `Prims::clear` unsets it,
    // so no state means nothing is queued. This runs after every geometry command, and the
    // moves below carry the whole `Prims` out and back.
    if prims.state.is_none() {
        return;
    }
    let mut taken = std::mem::take(prims);
    if let Some(ds) = taken.state.filter(|_| !taken.tris.is_empty() || !taken.lines.is_empty() || !taken.aa.is_empty() || !taken.discs.is_empty() || !taken.clears.is_empty()) {
        b.counts.draws += !(taken.tris.is_empty() && taken.lines.is_empty() && taken.aa.is_empty() && taken.discs.is_empty()) as u64;
        b.counts.triangles += (taken.tris.len() + taken.lines.len() + taken.aa.iter().map(|p| p.n as usize - 2).sum::<usize>() + 2 * taken.discs.len()) as u64;
        b.counts.clears += taken.clears.len() as u64;
        if let Some(ms) = &ds.samples {
            b.counts.multisampled += 1;
            b.counts.alpha_to_mask += ms.alpha_to_mask as u64;
            b.counts.sample_mask += (ms.cover != !0) as u64;
        }
        let drawable = !ds.clip.is_empty() && (ds.colours != 0 || (ds.depth_test && ds.depth_bits != 0) || (ds.stencil.enabled && ds.stencil_bits != 0));
        // The GPU arm takes the jobs itself, one op at a time; there are no tile workers.
        let gpu = b.store.is_gpu();
        let state = if gpu { 0 } else { b.tiles().state(ds) };
        if drawable {
            arrays(b.store, &ds);
        }
        // A poisoned scheduler refuses a job; it stays poisoned and the board is dead,
        // so a refusal carries nothing more.
        let run = |b: &mut Sink<'_>, t: &[raster::WVertex; 3], line: bool, shape: Shape<'_>| {
            if !drawable {
                b.counts.off_target += 1;
                return;
            }
            match raster::setup::shaped(t, &ds, state, shape) {
                Ok(mut job) => {
                    job.line = line;
                    if gpu {
                        b.store.apply(StoreOp::Tri { ds: &ds, job: &job });
                    } else {
                        let _ = b.tiles().submit(RasterJob::Tri(job));
                    }
                }
                Err(raster::setup::Skip::NoArea) => b.counts.no_area += 1,
                Err(raster::setup::Skip::NoSample) => b.counts.no_sample += 1,
                Err(raster::setup::Skip::Outside) => b.counts.off_target += 1,
            }
        };
        for t in &taken.tris {
            run(b, t, false, Shape::Plain);
        }
        for t in &taken.lines {
            run(b, t, true, Shape::Plain);
        }
        // A smoothed polygon or line: its fan, each triangle weighing its pixels by the whole outline.
        for poly in &taken.aa {
            let v = &taken.aa_verts[poly.start as usize..(poly.start + poly.n) as usize];
            let Some(outline) = raster::setup::Outline::new(v, &ds) else {
                b.counts.no_area += 1;
                continue;
            };
            for i in 1..v.len() - 1 {
                // The edges from the fan's apex and back to it are inner, but at its two ends.
                let inner = [i != 1, false, i != v.len() - 2];
                run(b, &[v[0], v[i], v[i + 1]], poly.line, Shape::Smooth { inner, outline: &outline });
            }
        }
        for d in &taken.discs {
            let shape = Shape::Disc { x: d.x, y: d.y, r: d.r };
            let q = d.quad;
            run(b, &[q[0], q[1], q[2]], false, shape);
            run(b, &[q[0], q[2], q[3]], false, shape);
        }
        for c in &taken.clears {
            let mut req = *c;
            req.colour = req.colour.filter(|_| ds.colours != 0);
            req.depth = req.depth.filter(|_| ds.depth_bits != 0);
            req.stencil = req.stencil.filter(|_| ds.stencil_bits != 0);
            if req.rect.is_empty() || (req.colour.is_none() && req.depth.is_none() && req.stencil.is_none()) {
                b.counts.off_target += 1;
                continue;
            }
            arrays(b.store, &ds);
            if gpu {
                b.store.apply(StoreOp::Clear { ds: &ds, req: &req });
            } else {
                let _ = b.tiles().submit(RasterJob::Clear(ClearJob { req, state }));
            }
        }
    }
    taken.clear();
    *prims = taken;
}
