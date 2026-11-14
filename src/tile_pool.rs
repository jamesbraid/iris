//! A pool of workers that draw a screen cut into square tiles, one owner per
//! tile.
//!
//! The producer writes each job into the next slot of a ring and appends the
//! slot's index to the queue of every tile the job's bounding box touches.
//! A tile with jobs waiting goes on a ready queue; a worker takes it and
//! owns it until its queue is empty, running the pool's closure on each job
//! in the order the producer wrote them. One owner per tile is the only
//! ordering: every pixel sees its jobs in order, and a large job is run by
//! as many workers as it covers tiles. The closure is given the job and the
//! tile and clips its work to the tile.
//!
//! A slot counts the tiles still to run it. The producer retires slots whose
//! count is zero in order (`tail`) and writes a slot again only after it has
//! retired, so a count of zero means no queue and no worker holds the slot.
//! A producer that finds the ring or a tile queue full runs ready tiles
//! itself while it waits.
//!
//! A bounding box must contain every pixel the job writes: a box too small
//! lets a pixel be written out of order, and the pool cannot check it.
//!
//! Tiles form a grid `GRID_SPAN` pixels across from the plane's origin;
//! coordinates wrap onto it, so pixels that far apart share a tile, which only
//! orders them. The closure sees the first copy of its tile and works out
//! which copies a job reaches (`copies`).

use parking_lot::{Condvar, Mutex};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::{Acquire, Relaxed, Release, SeqCst}};
use std::sync::Arc;
use std::thread::JoinHandle;

/// How long a thread spins before it yields or sleeps.
const SPINS: u32 = 2000;
/// How long an idle worker spins for a tile before it sleeps.
const IDLE_SPINS: u32 = 200;

/// The plane the tile grid covers, per axis.
pub const GRID_SPAN: i32 = 4096;
/// Slot indices the tile queues hold in all.
const QUEUE_BUDGET: usize = 1 << 22;

/// A rectangle in plane pixels, both corners inclusive; empty when a corner
/// is past the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// A tile, as the job closure sees it: the plane corner of its first copy on
/// the grid and its size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub x: i32,
    pub y: i32,
    pub size: i32,
}

/// The pool's sleep.
#[derive(Default)]
struct Waiter {
    sleepers: AtomicU32,
    m: Mutex<()>,
    cv: Condvar,
}

impl Waiter {
    /// Sleep unless `more` (read after the count is up) or exit.
    fn sleep(&self, more: impl Fn() -> bool, exit: &AtomicBool) {
        let mut g = self.m.lock();

        self.sleepers.fetch_add(1, SeqCst);
        if !more() && !exit.load(SeqCst) {
            self.cv.wait(&mut g);
        }
        self.sleepers.fetch_sub(1, SeqCst);
    }

    /// Wake a sleeper, if any; the caller has just published work with
    /// SeqCst.
    fn wake(&self, all: bool) {
        if self.sleepers.load(SeqCst) != 0 {
            let _g = self.m.lock();
            if all {
                self.cv.notify_all();
            } else {
                self.cv.notify_one();
            }
        }
    }
}

/// One tile's queue of slot indices: appended by the producer, run by the
/// worker that owns the tile.
#[repr(C, align(64))]
struct Queue {
    head: AtomicU64,
    tail: AtomicU64,
    /// On the ready queue or owned by a worker.
    scheduled: AtomicBool,
}

pub struct TilePool<J: Send, F: Fn(&J, Tile) + Sync> {
    ring: Box<[UnsafeCell<J>]>,
    /// Each slot's tiles still to run it.
    refs: Box<[AtomicU32]>,
    /// Slots written, and slots retired.
    staged: crossbeam_utils::CachePadded<AtomicU64>,
    tail: crossbeam_utils::CachePadded<AtomicU64>,
    tiles: Box<[Queue]>,
    queues: Box<[UnsafeCell<u32>]>,
    /// Tiles a grid row, log2 of the tile size, entries a tile queue.
    grid: usize,
    tile_log2: u32,
    queue: u64,
    ready: crossbeam_queue::ArrayQueue<u32>,
    run: F,
    pool: Waiter,
    exit: AtomicBool,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

// SAFETY: a slot is written by the producer before its index goes on any
// tile's queue and read-only after, until every tile has run it and the
// producer retires it. A tile queue's entry is written by the producer before
// it moves the tile's head past it and read by the tile's one owner after.
unsafe impl<J: Send, F: Fn(&J, Tile) + Sync> Sync for TilePool<J, F> {}
unsafe impl<J: Send, F: Fn(&J, Tile) + Sync> Send for TilePool<J, F> {}

impl<J: Default + Send + 'static, F: Fn(&J, Tile) + Send + Sync + 'static> TilePool<J, F> {
    /// Start `workers` threads named `name`-N on a ring of `slots` (a power
    /// of two) and tiles `tile` pixels square (clamped to a power of two, 8
    /// to 256); they run `run` on each job for each tile it touches.
    pub fn new(name: &str, slots: usize, tile: u32, workers: usize, run: F) -> Arc<Self> {
        assert!(slots.is_power_of_two() && slots >= 2, "a ring of {slots} slots");
        let tile = (tile.clamp(8, 256).next_power_of_two()) as i32;
        let grid = (GRID_SPAN / tile) as usize;
        let ntiles = grid * grid;
        let queue = (QUEUE_BUDGET / ntiles).clamp(16, 4096).next_power_of_two() as u64;
        let r = Arc::new(TilePool {
            ring: (0..slots).map(|_| UnsafeCell::new(J::default())).collect(),
            refs: (0..slots).map(|_| AtomicU32::new(0)).collect(),
            staged: Default::default(),
            tail: Default::default(),
            tiles: (0..ntiles).map(|_| Queue { head: AtomicU64::new(0), tail: AtomicU64::new(0), scheduled: AtomicBool::new(false) }).collect(),
            queues: (0..ntiles * queue as usize).map(|_| UnsafeCell::new(0)).collect(),
            grid,
            tile_log2: tile.trailing_zeros(),
            queue,
            ready: crossbeam_queue::ArrayQueue::new(ntiles),
            run,
            pool: Waiter::default(),
            exit: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
        });
        let mut threads = r.threads.lock();
        for i in 0..workers.max(1) {
            let c = r.clone();

            threads.push(std::thread::Builder::new().name(format!("{name}-{i}")).spawn(move || c.run_worker()).expect("tile pool thread"));
        }
        drop(threads);
        r
    }
}

impl<J: Send, F: Fn(&J, Tile) + Sync> TilePool<J, F> {
    // The producer's side.

    /// Retire the slots every tile has run, in order; the new tail.
    pub fn retire(&self) -> u64 {
        let mut t = self.tail.load(Relaxed);
        let staged = self.staged.load(Relaxed);
        let slots = self.ring.len() as u64;

        while t < staged && self.refs[(t & (slots - 1)) as usize].load(Acquire) == 0 {
            t += 1;
        }
        self.tail.store(t, Release);
        t
    }

    /// Slots written so far.
    #[inline]
    pub fn staged(&self) -> u64 {
        self.staged.load(Relaxed)
    }

    /// Wait while `busy`, running ready tiles meanwhile.
    #[cold]
    fn wait(&self, busy: impl Fn(&Self) -> bool) {
        let mut spins = 0u32;
        loop {
            self.retire();
            if !busy(self) {
                return;
            }
            if let Some(t) = self.ready.pop() {
                self.run_tile(t);
                continue;
            }
            spins += 1;
            if spins < SPINS {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    /// The next slot to write, once retired.
    #[inline]
    fn slot(&self) -> &mut J {
        let staged = self.staged.load(Relaxed);
        let slots = self.ring.len() as u64;

        if staged - self.tail.load(Relaxed) >= slots {
            self.wait(|r| staged - r.tail.load(Relaxed) >= slots);
        }
        // SAFETY: the slot is retired and on no tile's queue: only the
        // producer reaches it.
        unsafe { &mut *self.ring[(staged & (slots - 1)) as usize].get() }
    }

    /// The tile holding plane pixel (x, y).
    #[inline(always)]
    fn tile_of(&self, x: i32, y: i32) -> usize {
        let m = self.grid - 1;
        ((y >> self.tile_log2) as usize & m) * self.grid + ((x >> self.tile_log2) as usize & m)
    }

    /// Write `job` into the next slot and queue it on the tiles under
    /// `bbox`.
    #[inline]
    pub fn push(&self, job: J, bbox: Rect) {
        *self.slot() = job;
        self.commit(bbox);
    }

    /// Commit the slot just written to the tiles under `bbox`.
    fn commit(&self, bbox: Rect) {
        let Rect { x0, y0, x1, y1 } = bbox;
        let seq = self.staged.load(Relaxed);
        let idx = (seq & (self.ring.len() as u64 - 1)) as usize;
        let (tx0, ty0, tx1, ty1) = (x0 >> self.tile_log2, y0 >> self.tile_log2, x1 >> self.tile_log2, y1 >> self.tile_log2);
        let n = if x0 > x1 || y0 > y1 {
            0
        } else {
            // A rectangle wider than the grid wraps onto every column once.
            ((tx1 - tx0 + 1).min(self.grid as i32) * (ty1 - ty0 + 1).min(self.grid as i32)) as u32
        };

        self.refs[idx].store(n, Relaxed);
        self.staged.store(seq + 1, Release);
        if n == 0 {
            return;
        }
        for ty in ty0..=ty1.min(ty0 + self.grid as i32 - 1) {
            for tx in tx0..=tx1.min(tx0 + self.grid as i32 - 1) {
                self.enqueue(self.tile_of(tx << self.tile_log2, ty << self.tile_log2), idx as u32);
            }
        }
    }

    /// Append slot idx to tile t's queue, and put the tile on the ready
    /// queue unless it is there or owned.
    #[inline]
    fn enqueue(&self, t: usize, idx: u32) {
        let tile = &self.tiles[t];
        let head = tile.head.load(Relaxed);

        if head - tile.tail.load(Acquire) >= self.queue {
            self.wait(|r| head - r.tiles[t].tail.load(Acquire) >= r.queue);
        }
        // SAFETY: the entry is past the tile's tail: only the producer
        // reaches it until the head moves past it.
        unsafe { *self.queues[t * self.queue as usize + (head & (self.queue - 1)) as usize].get() = idx };
        tile.head.store(head + 1, SeqCst);
        if !tile.scheduled.swap(true, SeqCst) {
            self.ready.push(t as u32).expect("a tile on the ready queue once");
            self.pool.wake(false);
        }
    }

    /// Whether every slot written has run on all its tiles.
    pub fn idle(&self) -> bool {
        self.retire();
        self.tail.load(Acquire) == self.staged.load(Relaxed)
    }

    /// Wait until every slot written has run; the producer runs tiles
    /// meanwhile.
    pub fn drain(&self) {
        let target = self.staged.load(Relaxed);
        self.wait(|r| r.tail.load(Acquire) != target);
    }

    /// Stop the pool once everything has run.
    pub fn stop(&self) {
        self.drain();
        self.exit.store(true, SeqCst);
        self.pool.wake(true);
        for t in self.threads.lock().drain(..) {
            let _ = t.join();
        }
    }

    // The pool.

    /// Workers asleep on the wake condition, for the test of that path.
    #[cfg(test)]
    fn sleeping(&self) -> u32 {
        self.pool.sleepers.load(SeqCst)
    }

    fn run_worker(&self) {
        let mut spins = 0u32;

        loop {
            if let Some(t) = self.ready.pop() {
                spins = 0;
                self.run_tile(t);
                continue;
            }
            if self.exit.load(SeqCst) {
                return;
            }
            spins += 1;
            if spins < IDLE_SPINS {
                std::hint::spin_loop();
            } else {
                self.pool.sleep(|| !self.ready.is_empty(), &self.exit);
            }
        }
    }

    /// Own tile t: run its jobs in order until its queue is empty.
    fn run_tile(&self, t: u32) {
        let q = &self.tiles[t as usize];
        let size = 1i32 << self.tile_log2;
        // The tile's plane rectangle (its first copy on the grid).
        let tile = Tile { x: (t as usize % self.grid) as i32 * size, y: (t as usize / self.grid) as i32 * size, size };

        loop {
            let head = q.head.load(Acquire);
            let mut c = q.tail.load(Relaxed);

            while c < head {
                // SAFETY: the producer wrote the entry before moving the
                // head past it.
                let idx = unsafe { *self.queues[t as usize * self.queue as usize + (c & (self.queue - 1)) as usize].get() };
                // SAFETY: on this tile's queue, so written and not yet
                // retired.
                (self.run)(unsafe { &*self.ring[idx as usize].get() }, tile);
                self.refs[idx as usize].fetch_sub(1, Release);
                c += 1;
                q.tail.store(c, Release);
            }
            q.scheduled.store(false, SeqCst);
            if q.head.load(SeqCst) == c || q.scheduled.swap(true, SeqCst) {
                return;
            }
        }
    }
}

/// The copies of `tile` that rectangle `r` overlaps, by their plane corners:
/// one, unless the rectangle is wider or taller than the grid.
pub fn copies(r: Rect, tile: Tile) -> impl Iterator<Item = (i32, i32)> {
    let size = tile.size;
    // The first copy whose tile reaches lo, then one a grid span on.
    let axis = move |lo: i32, hi: i32, p: i32| {
        let first = p + GRID_SPAN * (lo - size + 1 - p).div_euclid(GRID_SPAN) + if (lo - size + 1 - p).rem_euclid(GRID_SPAN) != 0 { GRID_SPAN } else { 0 };
        std::iter::successors(Some(first), |&c| Some(c + GRID_SPAN)).take_while(move |&c| c <= hi)
    };
    axis(r.y0, r.y1, tile.y).flat_map(move |ty| axis(r.x0, r.x1, tile.x).map(move |tx| (tx, ty)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy, Default)]
    struct Job {
        seq: u32,
        rect: (i32, i32, i32, i32),
    }

    /// Every tile a job touches sees it once, and each tile sees its jobs in
    /// the order they were pushed, across several ring wraps.
    #[test]
    fn each_tile_runs_its_jobs_in_order() {
        const TILE: i32 = 64;
        let last: Arc<Vec<AtomicU32>> = Arc::new((0..64 * 64).map(|_| AtomicU32::new(0)).collect());
        let runs = Arc::new(AtomicUsize::new(0));
        let (l, n) = (last.clone(), runs.clone());
        let pool = TilePool::new("tile-test", 16, TILE as u32, 4, move |j: &Job, t: Tile| {
            let slot = (t.y / TILE) as usize * 64 + (t.x / TILE) as usize;
            let prev = l[slot].swap(j.seq, SeqCst);
            assert!(prev < j.seq, "tile ({},{}) saw {} after {}", t.x, t.y, j.seq, prev);
            // The job reaches this tile.
            let (x0, y0, x1, y1) = j.rect;
            assert!(x0 < t.x + t.size && x1 >= t.x && y0 < t.y + t.size && y1 >= t.y);
            n.fetch_add(1, Relaxed);
        });
        let mut want = 0;
        for seq in 1..=1000u32 {
            let x0 = (seq as i32 * 37) % 900;
            let y0 = (seq as i32 * 91) % 700;
            let (x1, y1) = (x0 + (seq as i32 % 300), y0 + (seq as i32 % 150));
            want += ((x1 / TILE - x0 / TILE + 1) * (y1 / TILE - y0 / TILE + 1)) as usize;
            pool.push(Job { seq, rect: (x0, y0, x1, y1) }, Rect { x0, y0, x1, y1 });
        }
        pool.drain();
        assert!(pool.idle());
        assert_eq!(pool.staged(), 1000);
        assert_eq!(runs.load(SeqCst), want);
        pool.stop();
    }

    /// A pool that has gone to sleep is woken by a job.
    #[test]
    fn a_sleeping_pool_is_woken_by_a_job() {
        let runs = Arc::new(AtomicUsize::new(0));
        let n = runs.clone();
        let pool = TilePool::new("tile-test", 4, 16, 2, move |_: &Job, _: Tile| {
            n.fetch_add(1, Relaxed);
        });
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while pool.sleeping() < 2 && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(pool.sleeping(), 2, "both workers went to sleep");
        pool.push(Job::default(), Rect { x0: 0, y0: 0, x1: 19, y1: 19 });
        pool.drain();
        assert_eq!(runs.load(SeqCst), 4);
        pool.stop();
    }

    /// An empty box runs nowhere and retires at once.
    #[test]
    fn empty_box_retires() {
        let pool = TilePool::new("tile-test", 4, 64, 1, |_: &Job, _: Tile| panic!("ran"));
        for _ in 0..20 {
            pool.push(Job::default(), Rect { x0: 5, y0: 0, x1: 4, y1: 0 });
        }
        assert!(pool.idle());
        pool.stop();
    }

    /// A box wider than the grid runs once on each tile of the row, and
    /// `copies` finds both copies of a tile it covers twice.
    #[test]
    fn wide_box_wraps_once_per_tile() {
        let runs = Arc::new(AtomicUsize::new(0));
        let n = runs.clone();
        let pool = TilePool::new("tile-test", 4, 256, 2, move |_: &Job, _: Tile| {
            n.fetch_add(1, Relaxed);
        });
        pool.push(Job::default(), Rect { x0: 0, y0: 0, x1: 2 * GRID_SPAN, y1: 10 });
        pool.drain();
        assert_eq!(runs.load(SeqCst), 16);
        pool.stop();
        let t = Tile { x: 0, y: 0, size: 256 };
        let c: Vec<_> = copies(Rect { x0: 0, y0: 0, x1: 2 * GRID_SPAN, y1: 10 }, t).collect();
        assert_eq!(c, vec![(0, 0), (GRID_SPAN, 0), (2 * GRID_SPAN, 0)]);
    }
}
