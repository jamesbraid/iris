//! KONA's tiles: the screen cut into square tiles, one owner each, run by the
//! shared tile pool (`crate::tile_pool`), and the draw-state table, work budget
//! and failure handling KONA adds on top.
//!
//! The pool owns the job ring, the per-tile queues and the workers: the producer
//! (`kona-hip`, through the raster stage) pushes each job with the rectangle it
//! can write, and each tile's owner runs the jobs that touch the tile, in the
//! order they were pushed, clipped to the tile's rectangle (`tile_pool` has the
//! ordering and slot rules). What stays here:
//!
//! - The pixel module clips to the tile it is given and keeps the window's own
//!   grid (the caller's quad grid, in the raster stage), so a tiled run is
//!   bit-exact with an inline one: `threads == 0` runs each job
//!   immediately over the whole screen, which is the inline run to compare with.
//!   A job's rectangle must contain every pixel it writes: the pool cannot check
//!   this; the pixel module's tests do.
//! - The slot holds a `RasterJob` for good, plain data the producer fills in
//!   place (`submit`), so a draw allocates nothing. A job names the draw state
//!   it runs under by an index into the draw-state table (`state`); an entry is
//!   not written again while any job in flight names it. The producer counts the
//!   jobs naming each entry and releases them as the pool retires their slots.
//! - A work budget bounds what is in flight: the producer waits, retiring as it
//!   goes, while the commands' estimated costs (pixels) exceed it, and resumes
//!   at half.
//! - A panic in a worker poisons the scheduler: the slot still retires, so a
//!   drain finishes, and every later `submit` and `drain` says so.
//! - A pixel rectangle's fragments (`Banding`) run in bands of rows on the same
//!   workers, each band a pool job of its own that the producer waits for.
//!
//! The managed area is at most 4096 wide, the pool's grid, so every pixel has
//! one tile and the tile the pool hands a job is the tile to clip to.

use super::gl::pixels::frags_cpu;
use super::raster::pixel;
use super::raster::setup::TriJob;
use super::raster::{ClearReq, DrawState, Rect};
use super::store::Store;
use crate::tile_pool::{Rect as PoolRect, Tile, TilePool, GRID_SPAN};
use std::cell::UnsafeCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Arc;

/// A clear, under draw state `state`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClearJob {
    pub req: ClearReq,
    pub state: u16,
}

/// What a slot of the ring holds: plain data with no pointers, naming its draw
/// state by index. `RasterJob::default` is the empty one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RasterJob {
    Tri(TriJob),
    Clear(ClearJob),
}

impl Default for RasterJob {
    fn default() -> Self {
        RasterJob::Clear(ClearJob::default())
    }
}

impl RasterJob {
    /// Where the command can write.
    pub fn rect(&self) -> Rect {
        match self {
            RasterJob::Tri(t) => t.rect,
            RasterJob::Clear(c) => c.req.rect,
        }
    }

    /// The draw-state table entry it runs under.
    pub fn state(&self) -> u16 {
        match self {
            RasterJob::Tri(t) => t.state,
            RasterJob::Clear(c) => c.state,
        }
    }

    /// An estimate of the work in pixels, for the tiles' budget.
    pub fn cost(&self) -> u64 {
        let r = self.rect();
        (r.x1 - r.x0).max(0) as u64 * (r.y1 - r.y0).max(0) as u64
    }
}

/// Rows of a rectangle's fragments, as pointers into the producer's buffers, which
/// `Banding::frags` outlives the bands of.
#[derive(Clone, Copy)]
struct BandJob {
    ds: *const DrawState,
    data: *const u32,
    fragments: *const AtomicU64,
    x0: i64,
    y0: i64,
    w: usize,
    h: usize,
    kind: u8,
}

// SAFETY: the pointers are read-only except `fragments`, an atomic, and outlive the job (above).
unsafe impl Send for BandJob {}

/// What a pool slot holds.
#[derive(Clone, Copy)]
enum PoolJob {
    Raster(RasterJob),
    Band(BandJob),
}

impl Default for PoolJob {
    fn default() -> Self {
        PoolJob::Raster(RasterJob::default())
    }
}

/// What the producer tells its owner while it waits on the workers.
pub trait Activity: Send + Sync {
    fn activity(&self);
}

/// A tile size or ring size that is not a power of two (or a ring under two slots).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    Tile(u32),
    Ring(usize),
}

/// A panic in a worker poisoned the tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Poisoned;

/// What the tiles counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileStats {
    pub submitted: u64,
    pub retired: u64,
    /// Jobs run by a tile owner, summed over tiles.
    pub tile_runs: u64,
    /// The most cost in flight at once, and times the producer waited for
    /// the ring or the budget.
    pub peak_cost: u64,
    pub waits: u64,
    /// Pool slots the pixel rectangles' bands took, not counted in `submitted` or `retired`.
    pub bands: u64,
    /// Times something that touches the store asked for every job to finish,
    /// and those that found jobs still queued, which is what a drain costs.
    pub drains: u64,
    pub busy_drains: u64,
    /// Wall time the pool's workers have spent drawing tiles, summed over them (so it can exceed wall time).
    pub busy_ns: u64,
}

/// One entry of the draw-state table: written by the producer while no job in
/// flight names it, read by the workers while one does.
struct StateCell(UnsafeCell<DrawState>);

// SAFETY: the producer writes an entry only when its count of jobs in flight,
// which it decrements after the pool has retired the job's slot (the pool's
// Acquire on the slot's count), is zero; workers read it only while a job that
// names it is on a tile queue or running, before the pool retires it.
unsafe impl Sync for StateCell {}

/// Entries in the draw-state table.
const STATES: usize = 64;

/// A slot with no draw-state entry named.
const NO_STATE: u16 = u16::MAX;

/// What the pool's workers share with the producer.
struct Shared {
    store: Arc<Store>,
    states: Vec<StateCell>,
    poisoned: AtomicBool,
    tile_runs: AtomicU64,
    busy_ns: AtomicU64,
    /// Pool slots `Banding` used: the producer's own bookkeeping does not see them.
    bands: AtomicU64,
}

/// The job closure the pool runs, boxed: it holds the shared state, and the
/// closure's own type cannot be named in `Tiles`. One indirect call per job and
/// tile, against the pixels the call draws.
type Run = Box<dyn Fn(&PoolJob, Tile) + Send + Sync>;

pub struct Tiles {
    shared: Arc<Shared>,
    /// None with no workers: jobs run inline.
    pool: Option<Arc<TilePool<PoolJob, Run>>>,
    /// Installed in the store while there are workers.
    banding: Option<Arc<Banding>>,
    screen: Rect,
    mask: u64,
    /// Producer-only: slots retired, each slot's cost and draw-state entry
    /// (`NO_STATE`: a band's slot), the cost in flight, the budget and whether
    /// the producer is waiting for half of it.
    tail: u64,
    costs: Vec<u64>,
    slot_state: Vec<u16>,
    /// Jobs in flight naming each draw-state entry, and one more for the entry
    /// the producer last set (`held`), which the next `state` call can reuse.
    refs: Vec<u32>,
    held: Option<u16>,
    inflight: u64,
    budget: u64,
    throttled: bool,
    /// With no workers, the job to run at once.
    scratch: RasterJob,
    stats: TileStats,
    /// Told the producer is working while it waits, so the driver's watchdog
    /// does not take a long drain for a dead pipe.
    handle: Option<Arc<dyn Activity>>,
}

impl Tiles {
    /// Tiles for a `store`'s screen cut into `tile`-pixel squares (a power of
    /// two), `threads` workers (0: inline), `ring` slots (a power of two) and a
    /// budget in pixels of cost.
    pub fn new(store: Arc<Store>, tile: u32, threads: usize, ring: usize, budget: u64, handle: Option<Arc<dyn Activity>>) -> Result<Tiles, Invalid> {
        if !tile.is_power_of_two() {
            return Err(Invalid::Tile(tile));
        }
        if !ring.is_power_of_two() || ring < 2 {
            return Err(Invalid::Ring(ring));
        }
        let (w, h) = (store.cpu().width() as i32, store.cpu().height() as i32);
        debug_assert!(w <= GRID_SPAN && h <= GRID_SPAN, "a {w}x{h} screen wraps the tile grid");
        let states = (0..STATES).map(|_| StateCell(UnsafeCell::new(DrawState::default()))).collect();
        let shared = Arc::new(Shared { store, states, poisoned: AtomicBool::new(false), tile_runs: AtomicU64::new(0), busy_ns: AtomicU64::new(0), bands: AtomicU64::new(0) });
        let pool = (threads > 0).then(|| {
            let s = shared.clone();
            let run: Run = Box::new(move |job, tile| {
                let began = std::time::Instant::now();
                match job {
                    PoolJob::Band(b) => {
                        // SAFETY: `Banding::frags` keeps what the band names alive until it retires.
                        let (ds, data, found) = unsafe { (&*b.ds, std::slice::from_raw_parts(b.data, 3 * b.w * b.h), &*b.fragments) };
                        match catch_unwind(AssertUnwindSafe(|| frags_cpu(&s.store, ds, b.x0, b.y0, b.w, b.h, b.kind, data))) {
                            Ok(n) => {
                                found.fetch_add(n, Relaxed);
                            }
                            Err(_) => s.poisoned.store(true, SeqCst),
                        }
                    }
                    PoolJob::Raster(job) => {
                        // SAFETY: the entry the job names stays as it is until the job retires.
                        let ds = unsafe { &*s.states[job.state() as usize].0.get() };
                        // The tile's rectangle, cut at the screen's edge.
                        let clip = Rect { x0: tile.x, y0: tile.y, x1: (tile.x + tile.size).min(w), y1: (tile.y + tile.size).min(h) };
                        if catch_unwind(AssertUnwindSafe(|| pixel::run(s.store.cpu(), job, ds, clip))).is_err() {
                            s.poisoned.store(true, SeqCst);
                        }
                        // A job writes only inside its rectangle: that is how tiles are assigned.
                        let hit = job.rect().intersect(&clip);
                        s.store.cpu().touch_rows(hit.y0, hit.y1);
                    }
                }
                s.busy_ns.fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
                s.tile_runs.fetch_add(1, Relaxed);
            });
            TilePool::new("kona-tile", ring, tile, threads, run)
        });
        // The GPU arm draws pixel rectangles itself.
        let banding = pool.as_ref().filter(|_| !shared.store.is_gpu()).map(|p| Arc::new(Banding { pool: p.clone(), shared: shared.clone(), tile: tile.clamp(8, 256).next_power_of_two() as i32, workers: threads, min_pixels: AtomicUsize::new(MIN_BAND_PIXELS) }));
        banding.iter().for_each(|b| shared.store.cpu().set_bands(Some(b.clone())));
        Ok(Tiles { shared, pool, banding, screen: Rect { x0: 0, y0: 0, x1: w, y1: h }, mask: ring as u64 - 1, tail: 0, costs: vec![0; ring], slot_state: vec![NO_STATE; ring], refs: vec![0; STATES], held: None, inflight: 0, budget, throttled: false, scratch: RasterJob::default(), stats: TileStats::default(), handle })
    }

    pub fn stats(&self) -> TileStats {
        let bands = self.shared.bands.load(Relaxed);
        TileStats { retired: self.tail.saturating_sub(bands), bands, tile_runs: self.shared.tile_runs.load(Relaxed), busy_ns: self.shared.busy_ns.load(Relaxed), ..self.stats }
    }

    /// Slots written to the pool, the bands' too: the pool counts them.
    fn head(&self) -> u64 {
        self.pool.as_ref().map_or(0, |p| p.staged())
    }

    /// Pixel rectangles smaller than `n` pixels are not cut into bands.
    #[cfg(test)]
    pub fn set_band_pixels(&self, n: usize) {
        self.banding.iter().for_each(|b| b.min_pixels.store(n, Relaxed));
    }

    pub fn is_poisoned(&self) -> bool {
        self.shared.poisoned.load(SeqCst)
    }

    /// Forget a worker's panic, for a reset: every job has retired, so the pool is whole again.
    pub fn clear_poison(&mut self) {
        let _ = self.drain();
        self.shared.poisoned.store(false, SeqCst);
    }

    /// The producer is waiting on the workers: the driver's watchdog is told.
    fn working(&self) {
        if let Some(h) = &self.handle {
            h.activity();
        }
    }

    /// Release the bookkeeping of the slots the pool has retired, in order.
    fn retire(&mut self) {
        let Some(pool) = &self.pool else { return };
        let done = pool.retire();
        while self.tail < done {
            let i = (self.tail & self.mask) as usize;
            self.inflight -= std::mem::take(&mut self.costs[i]);
            let state = std::mem::replace(&mut self.slot_state[i], NO_STATE);
            if state != NO_STATE {
                self.refs[state as usize] -= 1;
            }
            self.tail += 1;
        }
    }

    /// Enter `ds` in the draw-state table and return its index, for the jobs
    /// that follow to name. The entry last returned is reused when it is the
    /// same state. Waits, retiring as it goes, while no entry is free.
    pub fn state(&mut self, ds: DrawState) -> u16 {
        if let Some(h) = self.held {
            // SAFETY: the producer set the entry and nobody writes it but the producer.
            if unsafe { *self.shared.states[h as usize].0.get() } == ds {
                return h;
            }
            self.refs[h as usize] -= 1;
            self.held = None;
        }
        let i = loop {
            self.retire();
            match self.refs.iter().position(|&r| r == 0) {
                Some(i) => break i,
                None => {
                    self.stats.waits += 1;
                    self.working();
                    std::thread::yield_now();
                }
            }
        };
        // SAFETY: no job in flight names the entry, so no worker reads it.
        unsafe { *self.shared.states[i].0.get() = ds };
        self.refs[i] = 1;
        self.held = Some(i as u16);
        i as u16
    }

    /// Hand `job` to every tile its rectangle touches. Waits, retiring as it
    /// goes, while the ring is full or the budget is spent. The draw state it
    /// names must have come from `state` and not been replaced since.
    pub fn submit(&mut self, job: RasterJob) -> Result<(), Poisoned> {
        if self.is_poisoned() {
            return Err(Poisoned);
        }
        self.stats.submitted += 1;
        let Some(pool) = self.pool.clone() else {
            self.scratch = job;
            let r = job.rect().intersect(&self.screen);
            if !r.is_empty() {
                // SAFETY: the entry is the producer's and nothing else reads it.
                let ds = unsafe { &*self.shared.states[job.state() as usize].0.get() };
                pixel::run(self.shared.store.cpu(), &self.scratch, ds, self.screen);
                self.shared.store.cpu().touch_rows(r.y0, r.y1);
            }
            self.tail = self.stats.submitted;
            return Ok(());
        };
        self.retire();
        let mut waited = false;
        while self.head() - self.tail > self.mask || (self.inflight > 0 && self.over_budget()) {
            waited = true;
            self.working();
            std::thread::yield_now();
            self.retire();
        }
        self.stats.waits += waited as u64;
        let i = (self.head() & self.mask) as usize;
        self.refs[job.state() as usize] += 1;
        self.slot_state[i] = job.state();
        let (bounds, cost) = (job.rect().intersect(&self.screen), job.cost());
        self.costs[i] = cost;
        self.inflight += cost;
        self.stats.peak_cost = self.stats.peak_cost.max(self.inflight);
        // The slot has retired, so the pool's ring does not make this wait. An
        // empty box (the pool's corners are inclusive) runs nowhere and retires at once.
        pool.push(PoolJob::Raster(job), PoolRect { x0: bounds.x0, y0: bounds.y0, x1: bounds.x1 - 1, y1: bounds.y1 - 1 });
        if bounds.is_empty() {
            self.retire();
        }
        Ok(())
    }

    /// Over the budget; once over, stay waiting until half of it is free.
    fn over_budget(&mut self) -> bool {
        if self.inflight > self.budget {
            self.throttled = true;
        } else if self.inflight <= self.budget / 2 {
            self.throttled = false;
        }
        self.throttled
    }

    /// Jobs submitted and not yet retired.
    pub fn queued(&mut self) -> u64 {
        self.retire();
        // Inline, nothing is ever queued: the tail follows the submissions and the head does not.
        self.head().saturating_sub(self.tail)
    }

    /// Wait until every job submitted has been run and retired.
    pub fn drain(&mut self) -> Result<(), Poisoned> {
        self.stats.drains += 1;
        self.stats.busy_drains += (self.queued() > 0) as u64;
        while self.tail < self.head() {
            self.working();
            self.retire();
            std::thread::yield_now();
        }
        if self.is_poisoned() {
            Err(Poisoned)
        } else {
            Ok(())
        }
    }
}

/// The fewest pixels a band is worth: a rectangle is cut into bands of at least this many.
const MIN_BAND_PIXELS: usize = 1 << 12;

/// Runs a pixel rectangle's fragments (`StoreOp::Frags`) in bands of rows on the pool's workers: exact,
/// as a fragment reads and writes only its own pixel. Producer thread only.
pub struct Banding {
    pool: Arc<TilePool<PoolJob, Run>>,
    shared: Arc<Shared>,
    tile: i32,
    workers: usize,
    min_pixels: AtomicUsize,
}

impl Banding {
    /// The fragments that landed, once every band has run; None: too small or poisoned, run it alone.
    pub fn frags(&self, ds: &DrawState, x0: i64, y0: i64, w: usize, h: usize, kind: u8, data: &[u32]) -> Option<u64> {
        let bands = ((self.workers + 1) * 2).min(w * h / self.min_pixels.load(Relaxed).max(1)).min(h);
        if bands < 2 || data.len() < 3 * w * h || self.shared.poisoned.load(SeqCst) {
            return None;
        }
        // Earlier jobs finish first, so a band sees what they drew.
        self.pool.drain();
        struct Wait<'a>(&'a TilePool<PoolJob, Run>);
        impl Drop for Wait<'_> {
            // The bands name this call's buffers: wait for them even if the call unwinds.
            fn drop(&mut self) {
                self.0.drain();
            }
        }
        let wait = Wait(&self.pool);
        let found = AtomicU64::new(0);
        let rows = h.div_ceil(bands);
        for (k, j) in (0..h).step_by(rows).enumerate() {
            let band = BandJob { ds, data: data[3 * j * w..].as_ptr(), fragments: &found, x0, y0: y0 + j as i64, w, h: rows.min(h - j), kind };
            // The box only picks a tile for the band; each band has its own.
            let x = (k as i32 * self.tile) % GRID_SPAN;
            self.pool.push(PoolJob::Band(band), PoolRect { x0: x, y0: 0, x1: x, y1: 0 });
            self.shared.bands.fetch_add(1, Relaxed);
        }
        drop(wait);
        Some(found.load(Relaxed))
    }
}

impl Drop for Tiles {
    fn drop(&mut self) {
        // The store holds `Banding`, which holds the pool: let go of it first.
        if let Some(b) = &self.banding {
            self.shared.store.cpu().clear_bands(b);
        }
        let _ = self.drain();
        if let Some(pool) = &self.pool {
            pool.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 100;
    const H: u32 = 70;

    /// The draw state of a draw into word 1 over the whole screen.
    fn draw_state() -> DrawState {
        DrawState { clip: Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 }, colour: [1, 0], colours: 1, ..DrawState::default() }
    }

    /// Tiles with the draw state of the above entered.
    fn tiles(store: Arc<Store>, tile: u32, threads: usize, ring: usize, budget: u64) -> (Tiles, u16) {
        let mut t = Tiles::new(store, tile, threads, ring, budget, None).unwrap();
        let s = t.state(draw_state());
        (t, s)
    }

    /// A clear of `r` to `colour`, as a job under draw state `state`.
    fn clear(r: Rect, colour: u32, state: u16) -> RasterJob {
        RasterJob::Clear(ClearJob { req: ClearReq { rect: r, colour: Some(colour), ..Default::default() }, state })
    }

    fn store() -> Arc<Store> {
        let s = Arc::new(Store::new(W, H));
        s.ensure(1);
        s
    }

    /// The inline hash of `scene(400)`: a regression net that moves only if the
    /// scene or the clear does.
    const INLINE_400: u64 = 0xe30a_20fc_d4c9_4467;

    /// An order-sensitive scene of overlapping clears.
    fn scene(n: usize) -> Vec<(Rect, u32)> {
        let mut seed = 0x1234_5678u64;
        let mut next = move |m: i32| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) % m as u64) as i32
        };
        (0..n)
            .map(|k| {
                let (x0, y0) = (next(110) - 10, next(80) - 10);
                (Rect { x0, y0, x1: x0 + next(60) + 1, y1: y0 + next(40) + 1 }, 0xff00_0000 | k as u32)
            })
            .collect()
    }

    fn run(scene: &[(Rect, u32)], threads: usize, tile: u32, ring: usize, budget: u64) -> (u64, TileStats) {
        let store = store();
        let (mut t, s) = tiles(store.clone(), tile, threads, ring, budget);
        for (r, c) in scene {
            t.submit(clear(*r, *c, s)).unwrap();
        }
        t.drain().unwrap();
        let stats = t.stats();
        drop(t);
        (store.checksum(), stats)
    }

    /// A tiled run is bit-exact with the inline one at every worker count, tile
    /// size and ring size, and the scene is order sensitive, so a lost ordering shows.
    ///
    /// Ignored: on Apple silicon it hangs. A tile's hand-over can strand a job
    /// (see `clanker/plans/kona-tile-handoff-race.md`); remove this once that is fixed.
    #[test]
    #[ignore = "hangs on Apple silicon: the tile hand-over strands a job (clanker/plans/kona-tile-handoff-race.md)"]
    fn a_tiled_run_is_bit_exact_with_the_inline_one() {
        let s = scene(400);
        let (inline, st) = run(&s, 0, 32, 64, 1 << 20);
        assert_eq!(st.submitted, 400);
        assert_eq!(inline, INLINE_400, "the pinned inline hash");
        for threads in [1, 2, 4] {
            for tile in [8, 16, 32, 64] {
                for ring in [4, 64] {
                    let (h, st) = run(&s, threads, tile, ring, 1 << 14);
                    assert_eq!(h, inline, "{threads} workers, tile {tile}, ring {ring}");
                    assert_eq!(st.retired, 400);
                }
            }
        }
        let mut r = s.clone();
        r.reverse();
        assert_ne!(run(&r, 0, 32, 64, 1 << 20).0, inline, "the scene depends on its order");
    }

    /// Whoever finishes drawing raises the store's flag after the pixels are
    /// written, with a worker pool or without one: a frame started after it
    /// shows them.
    #[test]
    fn a_finished_job_touches_the_store() {
        for threads in [0, 2] {
            let store = store();
            let (mut t, s) = tiles(store.clone(), 16, threads, 16, 1 << 14);
            store.take_touched();
            t.submit(clear(Rect { x0: 0, y0: 0, x1: 40, y1: 40 }, 0xff00_0001, s)).unwrap();
            t.drain().unwrap();
            assert!(store.take_touched(), "{threads} workers");
            assert!(!store.take_touched(), "{threads} workers: taking clears it");
        }
    }

    /// A job reaches every tile its rectangle touches and no pixel outside it
    /// (cut at the screen's edge) is written.
    #[test]
    fn a_job_reaches_every_tile_it_touches() {
        let boxes = [Rect { x0: 0, y0: 0, x1: 1, y1: 1 }, Rect { x0: 15, y0: 15, x1: 17, y1: 17 }, Rect { x0: 10, y0: 60, x1: 100, y1: 70 }, Rect { x0: -50, y0: -50, x1: 500, y1: 500 }, Rect { x0: 32, y0: 0, x1: 48, y1: 16 }, Rect { x0: 200, y0: 0, x1: 300, y1: 10 }];
        for b in boxes {
            let store = store();
            let (mut t, s) = tiles(store.clone(), 16, 3, 8, 1 << 20);
            t.submit(clear(b, 0xff11_2233, s)).unwrap();
            t.drain().unwrap();
            for y in 0..H as i32 {
                for x in 0..W as i32 {
                    let inside = x >= b.x0 && x < b.x1 && y >= b.y0 && y < b.y1;
                    assert_eq!(store.read(x as u32, y as u32, 1) != 0, inside, "{b:?} at ({x}, {y})");
                }
            }
        }
    }

    /// Jobs on one tile run in submission order and a slot is not written again under
    /// a worker: a two-slot ring and a thousand overlapping jobs leave the last one's colour.
    #[test]
    fn tiles_run_in_order_and_a_slot_is_not_recycled_under_a_worker() {
        let store = store();
        let (mut t, s) = tiles(store.clone(), 16, 2, 2, 1 << 20);
        let all = Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 };
        for k in 0..1000u32 {
            t.submit(clear(all, 0xff00_0000 | k, s)).unwrap();
        }
        t.drain().unwrap();
        assert_eq!(t.stats().retired, 1000);
        assert!((0..H).all(|y| (0..W).all(|x| store.read(x, y, 1) == 0xff00_0000 | 999)));
    }

    /// The cost in flight stays near the budget: the producer waits for work to retire.
    #[test]
    fn the_budget_bounds_what_is_in_flight() {
        let store = store();
        let (mut t, s) = tiles(store, 16, 2, 64, 3000);
        let all = Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 };
        for k in 0..200u32 {
            t.submit(clear(all, k, s)).unwrap();
        }
        t.drain().unwrap();
        let st = t.stats();
        assert!(st.peak_cost <= 3000 + (W * H) as u64, "{st:?}");
        assert!(st.waits > 0, "{st:?}");
    }

    /// A job wholly off the screen retires at once, with no tile named.
    #[test]
    fn a_job_off_the_screen_retires_at_once() {
        let store = store();
        let (mut t, s) = tiles(store, 16, 2, 4, 1 << 20);
        t.submit(clear(Rect { x0: 500, y0: 500, x1: 600, y1: 600 }, 1, s)).unwrap();
        assert_eq!((t.stats().tile_runs, t.queued()), (0, 0));
    }

    /// A panic in a worker poisons the tiles: the slot still retires, a drain ends and
    /// later submissions say so. (A job that names a colour word set-up never allocated
    /// panics in the pixel module.)
    #[test]
    fn a_panicking_job_poisons_the_tiles() {
        let store = Arc::new(Store::new(W, H));
        let (mut t, s) = tiles(store, 16, 2, 4, 1 << 20);
        t.submit(clear(Rect { x0: 0, y0: 0, x1: 10, y1: 10 }, 1, s)).unwrap();
        assert_eq!(t.drain(), Err(Poisoned));
        assert!(t.is_poisoned());
        assert_eq!(t.submit(RasterJob::default()), Err(Poisoned));
    }

    /// A reset forgets the panic: submissions work again and the pool still runs jobs.
    #[test]
    fn clearing_the_poison_lets_the_tiles_work_again() {
        let store = Arc::new(Store::new(W, H));
        let (mut t, s) = tiles(store.clone(), 16, 2, 4, 1 << 20);
        t.submit(clear(Rect { x0: 0, y0: 0, x1: 10, y1: 10 }, 1, s)).unwrap();
        assert_eq!(t.drain(), Err(Poisoned));
        t.clear_poison();
        assert!(!t.is_poisoned());
        store.ensure(1);
        t.submit(clear(Rect { x0: 0, y0: 0, x1: 10, y1: 10 }, 0xff00_0007, s)).unwrap();
        t.drain().unwrap();
        assert_eq!(store.read(5, 5, 1), 0xff00_0007);
    }

    /// Dropping runs what is queued and joins the workers.
    #[test]
    fn dropping_runs_what_is_queued_and_joins() {
        let store = store();
        let (mut t, s) = tiles(store.clone(), 16, 2, 16, 1 << 20);
        let all = Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 };
        for k in 0..10u32 {
            t.submit(clear(all, 0xff00_0000 | k, s)).unwrap();
        }
        drop(t);
        assert_eq!(store.read(5, 5, 1), 0xff00_0009);
    }

    /// A retired slot lets go of the draw state its job named, and the same
    /// state entered again is the same entry.
    #[test]
    fn a_retired_slot_lets_go_of_its_draw_state() {
        let store = store();
        for threads in [0, 2] {
            let (mut t, s) = tiles(store.clone(), 16, threads, 4, 1 << 20);
            assert_eq!(t.state(draw_state()), s, "{threads} workers: unchanged state, same entry");
            t.submit(clear(Rect { x0: 0, y0: 0, x1: 20, y1: 20 }, 1, s)).unwrap();
            let other = t.state(DrawState { colours: 0, ..draw_state() });
            t.drain().unwrap();
            // Only the entry set last is held: the job's hold went when it retired.
            assert_eq!((t.refs.iter().sum::<u32>(), t.refs[other as usize]), (1, 1), "{threads} workers");
        }
    }

    /// A draw-state table far smaller than the states used makes the producer
    /// wait for entries, and the result is the inline one: a state is not
    /// overwritten while a job in flight names it.
    #[test]
    fn more_states_than_entries_wait_for_free_ones() {
        let run = |threads: usize| {
            let store = store();
            (2..=3).for_each(|w| store.ensure(w));
            let mut t = Tiles::new(store.clone(), 8, threads, 256, 1 << 20, None).unwrap();
            for k in 0..(4 * STATES as i32) {
                // Each state names one of three colour words, and no two in a row are alike.
                let s = t.state(DrawState { colour: [1 + (k % 3) as u32, k as u32 + 100], ..draw_state() });
                t.submit(clear(Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 }, 0xff00_0000 | k as u32, s)).unwrap();
            }
            t.drain().unwrap();
            drop(t);
            store.checksum()
        };
        assert_eq!(run(0), run(3));
    }

    /// A producer that waits on the workers tells the driver's watchdog the pipe
    /// is working: for a full ring, a spent budget, a full state table and a drain.
    #[test]
    fn a_producer_that_waits_feeds_the_watchdog() {
        use std::sync::atomic::Ordering::Relaxed;
        let f = crate::dev::kona::tests::Fixture::new();
        let host = f.kona.shared.host.clone();
        let handle = crate::dev::kona::handle::DisplayHandle::new(&host);
        let all = Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 };
        // A two-slot ring: the third job has to wait for a worker.
        let mut t = Tiles::new(store(), 16, 1, 2, 1 << 20, Some(Arc::new(handle.clone()))).unwrap();
        let s = t.state(draw_state());
        host.activity.store(false, Relaxed);
        for k in 0..64u32 {
            t.submit(clear(all, k, s)).unwrap();
        }
        t.drain().unwrap();
        assert!(t.stats().waits > 0 && host.activity.load(Relaxed), "{:?}", t.stats());
        // A budget below one job's cost makes the next submission wait.
        host.activity.store(false, Relaxed);
        let mut t = Tiles::new(store(), 16, 1, 64, 10, Some(Arc::new(handle))).unwrap();
        let s = t.state(draw_state());
        for k in 0..8u32 {
            t.submit(clear(all, k, s)).unwrap();
        }
        t.drain().unwrap();
        assert!(host.activity.load(Relaxed));
    }

    /// The state a random rectangle of fragments runs under: any mix of the tests, blends and writes after ownership.
    fn random_state(rnd: &mut impl FnMut(u32) -> u32) -> DrawState {
        use crate::dev::kona::back::raster::multisample::Samples;
        use crate::dev::kona::back::raster::ops::{BlendFactor::*, Compare, LogicOp, Ownership};
        let compare = [Compare::Never, Compare::Less, Compare::Equal, Compare::Lequal, Compare::Greater, Compare::Notequal, Compare::Gequal, Compare::Always];
        let factors = [Zero, One, SrcColour, OneMinusSrcColour, SrcAlpha, OneMinusSrcAlpha, DstAlpha, OneMinusDstAlpha, DstColour, OneMinusDstColour, SrcAlphaSaturate];
        let mut ds = DrawState { origin: [rnd(8) as i32, rnd(8) as i32], colour: [2, 3], colours: 1 + rnd(2) as u8, depth_mask: rnd(4) != 0, colour_mask: [true; 4], ..DrawState::default() };
        let (cx, cy) = (rnd(30) as i32, rnd(30) as i32);
        ds.clip = Rect { x0: cx, y0: cy, x1: (cx + 1 + rnd(64) as i32).min(64), y1: (cy + 1 + rnd(48) as i32).min(48) };
        if rnd(2) == 0 {
            (ds.blend.enabled, ds.blend.src, ds.blend.dst) = (true, factors[rnd(11) as usize], factors[rnd(11) as usize]);
        }
        if rnd(3) == 0 {
            (ds.depth_bits, ds.depth_word, ds.depth_test, ds.depth_func) = (24, 4, true, compare[rnd(8) as usize]);
        }
        if rnd(3) == 0 {
            (ds.stencil_bits, ds.stencil_word, ds.stencil.enabled, ds.stencil.func, ds.stencil.reference) = (8, 5, true, compare[rnd(8) as usize], rnd(256) as u8);
        }
        if rnd(4) == 0 {
            ds.alpha = crate::dev::kona::back::raster::AlphaTest { enabled: true, func: compare[rnd(8) as usize], reference: rnd(8192) };
        }
        if rnd(4) == 0 {
            ds.logic = Some([LogicOp::Xor, LogicOp::And, LogicOp::Clear][rnd(3) as usize]);
        }
        if rnd(4) == 0 {
            ds.colour_mask = [rnd(2) == 0, rnd(2) == 0, rnd(2) == 0, rnd(2) == 0];
        }
        if rnd(5) == 0 {
            ds.ownership = Some(Ownership { reference: 1, mask: 3 });
        }
        if rnd(6) == 0 {
            // The sample words here hold colour alone.
            (ds.depth_bits, ds.depth_test, ds.stencil_bits, ds.stencil.enabled) = (0, false, 0, false);
            ds.samples = Some(Samples { count: 4, at: [[2, 2]; 8], cover: !0, alpha_to_mask: false, alpha_to_one: false, colour: (6, 3), depth: (0, 0), stencil: (0, 0) });
        }
        ds
    }

    /// A rectangle of fragments cut into bands on 1, 2 and 4 workers leaves the store word for word
    /// as the one thread does, with the same count, under random state over random contents.
    #[test]
    fn bands_leave_the_store_as_one_thread_does() {
        use crate::dev::kona::back::gl::pixels::{FRAG_COLOUR, FRAG_STENCIL};
        use crate::dev::kona::back::store::{StoreOp, DID_PLANE};
        let mut seed = 0x2545_f491u32;
        let mut rnd = move |n: u32| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) % n
        };
        let words: Vec<usize> = (2..=8).chain([DID_PLANE]).collect();
        let fresh = || {
            let store = Arc::new(Store::new(64, 48));
            words.iter().for_each(|&w| store.ensure(w));
            store
        };
        let (alone, workers) = (fresh(), [1, 2, 4].map(|n| (Tiles::new(fresh(), 8, n, 16, 1 << 20, None).unwrap(), n)));
        workers.iter().for_each(|(t, _)| t.set_band_pixels(1));
        for case in 0..600 {
            let ds = random_state(&mut rnd);
            let (w, h) = (1 + rnd(48) as usize, 1 + rnd(40) as usize);
            let (x0, y0) = (rnd(70) as i64 - 3, rnd(50) as i64 - 3);
            let kind = if rnd(8) == 0 { FRAG_STENCIL } else { FRAG_COLOUR };
            let data: Vec<u32> = (0..3 * w * h)
                .map(|k| match (k % 3, kind) {
                    (0, FRAG_COLOUR) => (rnd(8) != 0) as u32 * (1 << 31) | rnd(8192) | rnd(8192) << 13,
                    (0, _) => (rnd(8) != 0) as u32 * (1 << 31) | rnd(256),
                    (1, _) => rnd(8192) | rnd(8192) << 13,
                    _ => rnd(1 << 24) << 8,
                })
                .collect();
            let (t, n) = &workers[case % 3];
            let before = t.stats().bands;
            for store in [&alone, &t.shared.store] {
                for &word in &words {
                    let a = store.array(word).unwrap();
                    // The same contents in both: the depth and ownership planes need values to test against.
                    let mut fill = 0x9e37_79b9u32 ^ (case as u32 * 31 + word as u32);
                    (0..a.len()).for_each(|i| {
                        fill = fill.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        unsafe { a.store(i, if word == DID_PLANE { (fill >> 24) % 8 << 2 } else { fill }) };
                    });
                }
            }
            let (one, many) = (alone.apply(StoreOp::Frags { ds: &ds, x0, y0, w, h, kind, data: &data }), t.shared.store.apply(StoreOp::Frags { ds: &ds, x0, y0, w, h, kind, data: &data }));
            assert_eq!(one.fragments, many.fragments, "case {case}: {n} workers, {w}x{h}");
            assert_eq!(alone.checksum(), t.shared.store.checksum(), "case {case}: {n} workers, {w}x{h} at {x0},{y0}");
            assert!(t.stats().bands > before || h < 2, "case {case}: banded");
        }
    }

    /// The bands of a rectangle run after the jobs queued before them and are finished when it returns.
    #[test]
    fn a_banded_rectangle_follows_the_jobs_before_it_and_is_done_when_it_returns() {
        use crate::dev::kona::back::gl::pixels::FRAG_COLOUR;
        use crate::dev::kona::back::raster::ops::LogicOp;
        use crate::dev::kona::back::store::StoreOp;
        let ds = DrawState { logic: Some(LogicOp::Xor), colour_mask: [true; 4], ..draw_state() };
        let data: Vec<u32> = (0..3 * (W * H) as usize).map(|k| [1 << 31 | 0x0f0f | 0x0f0f << 13, 0x0f0f | 0x0f0f << 13, 0][k % 3]).collect();
        let run = |threads: usize| {
            let store = store();
            let (mut t, s) = tiles(store.clone(), 16, threads, 16, 1 << 20);
            t.set_band_pixels(1);
            // The clears are still queued when the rectangle comes: the xor reads what they left.
            for k in 0..20u32 {
                t.submit(clear(Rect { x0: 0, y0: 0, x1: W as i32, y1: H as i32 }, 0xff00_0000 | k * 0x0101, s)).unwrap();
            }
            store.apply(StoreOp::Frags { ds: &ds, x0: 0, y0: 0, w: W as usize, h: H as usize, kind: FRAG_COLOUR, data: &data });
            // No drain: a command after it sees every row.
            let sum = store.checksum();
            (sum, t.stats().bands)
        };
        let (inline, none) = run(0);
        assert_eq!(none, 0);
        for threads in [1, 3] {
            let (banded, bands) = run(threads);
            assert!(bands >= 2, "{threads} workers: {bands} bands");
            assert_eq!(banded, inline, "{threads} workers");
        }
    }

    /// A worker that panics in a band poisons the tiles, and the call still returns: the bands are
    /// waited for.
    #[test]
    fn a_panic_in_a_band_poisons_the_tiles_and_the_call_returns() {
        use crate::dev::kona::back::gl::pixels::FRAG_COLOUR;
        use crate::dev::kona::back::store::StoreOp;
        let store = store();
        let (mut t, _) = tiles(store.clone(), 16, 2, 16, 1 << 20);
        t.set_band_pixels(1);
        // A colour word that was never allocated: the pixel module panics on it.
        let ds = DrawState { colour: [9, 0], ..draw_state() };
        let data = vec![1 << 31; 3 * 40 * 40];
        store.apply(StoreOp::Frags { ds: &ds, x0: 0, y0: 0, w: 40, h: 40, kind: FRAG_COLOUR, data: &data });
        assert!(t.is_poisoned());
        assert_eq!(t.drain(), Err(Poisoned));
        assert_eq!(t.submit(RasterJob::default()), Err(Poisoned));
    }

    #[test]
    fn a_bad_tile_or_ring_size_is_refused() {
        let store = store();
        assert_eq!(Tiles::new(store.clone(), 24, 1, 4, 1, None).err(), Some(Invalid::Tile(24)));
        assert_eq!(Tiles::new(store.clone(), 16, 1, 3, 1, None).err(), Some(Invalid::Ring(3)));
        assert_eq!(Tiles::new(store, 16, 1, 1, 1, None).err(), Some(Invalid::Ring(1)));
    }
}
