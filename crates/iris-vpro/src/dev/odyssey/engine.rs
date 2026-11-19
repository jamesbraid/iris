//! The VPro engine as a caller drives it: aperture accesses in, events and
//! frames out, time and memory passed in, no emulator behind it.
//!
//! Everything below the register file is iris's own board code, mounted
//! unchanged; this file is the handle around it. Time is an argument
//! (`advance`), never read here. The board's host-side threads run only in
//! `Mode::Threaded`; `Mode::Inline` runs every store, draw and DMA on the
//! caller's thread inside `write`, which is what replays and parity tests
//! use.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use super::board::Board;
use super::mmio::BoardLocks;
use super::reader::{DisplayShared, ScanoutReader};
use crate::traits::BusDevice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No engine threads: work runs inside the call that queues it.
    Inline,
    /// The board's front-end and raster threads run behind the stores.
    Threaded,
}

pub struct Config {
    /// `v12`, `v10`, `v8` or `v6`.
    pub product: String,
    /// The widget number the board answers as (its identity register).
    pub widget: u32,
    pub mode: Mode,
    /// The CPU raster unit's workers and tile size, as iris's `[graphics]`.
    pub pool: (usize, u32),
    /// Draw on the GPU through this wgpu backend name ("auto", "vulkan"...).
    pub gpu: Option<String>,
}

/// What the engine says to the shell. Drained under the shell's own lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A crosstalk interrupt packet: the destination widget and address,
    /// the vector, and whether the source rose or fell.
    Interrupt { widget: u8, addr: u64, vector: u8, level: bool },
    /// The GPU unit failed; every access answers an error from now on.
    Fault(String),
}

/// Board memory the shell lends, as an aligned region it keeps alive for
/// the engine's life.
#[derive(Clone, Copy)]
pub struct Lent {
    pub ptr: *mut u64,
    pub bytes: u64,
}

// SAFETY: a lend is a promise about the region, not a borrow of it.
unsafe impl Send for Lent {}
unsafe impl Sync for Lent {}

/// A rectangle of board pixels, corners included.
#[derive(Clone, Copy, Debug)]
pub struct Box2 {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

type Notify = Box<dyn Fn() + Send + Sync>;

/// Where engine threads post. The hook only schedules; it is called once
/// per batch of events (the flag clears when the shell drains).
struct Outbox {
    events: Mutex<Vec<Event>>,
    notified: AtomicBool,
    notify: Mutex<Option<Notify>>,
}

impl Outbox {
    fn post(&self, e: Event) {
        self.events.lock().push(e);
        if !self.notified.swap(true, Ordering::AcqRel) {
            if let Some(n) = &*self.notify.lock() {
                n();
            }
        }
    }
}

pub struct Engine {
    locks: BoardLocks,
    out: Arc<Outbox>,
    /// The display's side of the scan-out: retrace clock and composed frame.
    reader: Mutex<ScanoutReader>,
    mode: Mode,
    now: AtomicI64,
    fault_sent: AtomicBool,
}

/// Board memory the product carries, in bytes, for a shell that lends it.
pub fn memory_bytes(product: &str) -> Result<u64, String> {
    let mut b = Box::new(Board::default());
    b.widget.revision = 0xa;
    b.widget.memory_size = 128;
    b.widget.memory_banks = 4;
    b.widget.cas_latency = 3;
    b.widget.backend_version = 1;
    b.widget.product = product.to_string();
    b.configure()?;
    Ok((b.widget.memory_size.max(32) as u64) << 20)
}

impl Engine {
    /// A board of `config.product`. `memory` is the region the shell lends
    /// for board memory (`memory_bytes` long) or None for one the engine
    /// allocates; `guest` is host memory the board's DMA reaches.
    ///
    /// # Safety
    /// A lent region is 8-byte aligned, valid for reads and writes for the
    /// engine's life, and written by nothing but the engine.
    pub unsafe fn new(config: Config, memory: Option<Lent>, guest: Arc<dyn BusDevice>) -> Result<Engine, String> {
        let lent = memory.map(|m| (m.ptr, m.bytes));
        // SAFETY: the caller's contract.
        let mut b = unsafe { Board::new_lent(&config.product, config.widget, lent)? };
        b.raster.set_pool(config.pool.0, config.pool.1);
        if let Some(backend) = &config.gpu {
            #[cfg(feature = "wgpu")]
            {
                let opts = crate::wgpu::GpuOptions { backend: backend.clone(), ..Default::default() };
                let gpu = crate::wgpu::Gpu::new(&opts, Box::new(|why| log::error!("iris-vpro: GPU failed: {why}")))?;
                b.enable_gpu(&gpu)?;
            }
            #[cfg(not(feature = "wgpu"))]
            return Err(format!("the GPU unit {backend:?} needs the wgpu feature"));
        }
        b.guest = Some(guest);
        let out = Arc::new(Outbox { events: Mutex::new(Vec::new()), notified: AtomicBool::new(false), notify: Mutex::new(None) });
        let sink = out.clone();
        b.front.lock().packets = Some(Arc::new(move |widget, addr, vector, level| {
            sink.post(Event::Interrupt { widget, addr, vector, level });
            true
        }));
        let locks = BoardLocks::new(b);
        let mut reader = ScanoutReader::default();
        reader.attach(&locks.shared);
        let e = Engine { locks, out, reader: Mutex::new(reader), mode: config.mode, now: AtomicI64::new(0), fault_sent: AtomicBool::new(false) };
        if config.mode == Mode::Threaded {
            e.locks.work_start();
        }
        Ok(e)
    }

    /// Power-on: reset the board. Before the first access.
    pub fn reset(&self) {
        // SAFETY: nothing else touches the board before the first access.
        let b = unsafe { &mut *self.locks.board.get() };
        b.reset();
    }

    /// Call `f` when events wait. It only schedules (a bottom half); it
    /// must not call back into the engine.
    pub fn set_notify(&self, f: Notify) {
        *self.out.notify.lock() = Some(f);
    }

    /// A store at aperture offset `off`. False when the bus would answer
    /// busy (threaded mode, FIFO full or a store that waits): retry after
    /// `advance`.
    #[inline]
    pub fn write(&self, off: u64, size: u32, val: u64) -> bool {
        self.locks.bus_write(off, val, size)
    }

    /// A load at `off`. None when it must wait for queued work (threaded
    /// mode): retry, or `quiesce` first. Inline mode never answers None.
    #[inline]
    pub fn read(&self, off: u64, size: u32) -> Option<u64> {
        self.locks.bus_read(off, size)
    }

    /// Run the retrace for every frame period due at `now_ns` (the
    /// caller's clock): swaps latch and the retrace interrupt is posted.
    /// Returns the time of the next retrace, if the mode has one.
    pub fn advance(&self, now_ns: i64) -> Option<i64> {
        self.now.store(now_ns, Ordering::Relaxed);
        if self.mode == Mode::Inline {
            // The GPU unit sends its mirror's changes up and submits its
            // open batch at the end of a run of stores; inline there is
            // no such end but this call. A board with no GPU unit does
            // nothing.
            // SAFETY: inline, the caller's thread is the board's only one.
            unsafe { &*self.locks.board.get() }.gpu_upload_tick(true);
        }
        let mut r = self.reader.lock();
        r.attach(&self.locks.shared);
        r.poll(now_ns);
        (r.clock.deadline >= 0).then_some(r.clock.deadline)
    }

    /// Whether nothing is queued: the FIFO and the raster queue are empty.
    pub fn idle(&self) -> bool {
        !self.locks.fifo.pending() && self.locks.raster.pending() == 0
    }

    /// Finish all queued work, as before a snapshot or reset. False if the
    /// board is still busy after `timeout`.
    pub fn quiesce(&self, timeout: std::time::Duration) -> bool {
        self.locks.quiesce(timeout).is_some()
    }

    /// Make the pixels of `b` readable in board memory: the raster queue
    /// drains, and on the GPU unit only the blocks drawn under `b` come
    /// back. False if the board stays busy.
    pub fn observe(&self, b: Box2) -> bool {
        match self.locks.quiesce(std::time::Duration::from_secs(5)) {
            Some(g) => {
                let _ = g.planes_in(b.x0 as i64, b.y0 as i64, b.x1 as i64, b.y1 as i64);
                true
            }
            None => false,
        }
    }

    /// Move the events waiting into `out`.
    pub fn drain_events(&self, out: &mut Vec<Event>) {
        out.append(&mut self.out.events.lock());
        self.out.notified.store(false, Ordering::Release);
        if let Some(f) = self.locks.gpu_fault() {
            if !self.fault_sent.swap(true, Ordering::AcqRel) {
                out.push(Event::Fault(f));
            }
        }
    }

    /// The scan-out composed from board memory as the registers describe
    /// it, 0x00RRGGBB, `(width, height, pixels)`; None with no mode or a
    /// board that stays busy. Drains the board first, and with the GPU
    /// unit brings all of its memory back to the host's copy.
    pub fn frame(&self) -> Option<(u32, u32, Vec<u32>)> {
        let g = self.locks.quiesce(std::time::Duration::from_secs(5))?;
        g.gpu_upload_tick(true);
        g.gpu_sync();
        drop(g);
        let mut r = self.reader.lock();
        r.attach(&self.locks.shared);
        r.recompose();
        r.update_to(None);
        // The reader composes 0xAABBGGRR; this API hands out 0x00RRGGBB.
        let rgb = |p: &u32| (p & 0xff) << 16 | (p & 0xff00) | (p >> 16) & 0xff;
        (r.frame_width != 0).then(|| (r.frame_width, r.frame_height, r.frame.iter().map(rgb).collect()))
    }

    /// The GPU unit's counters (batches, records, waits, readbacks) as
    /// text; None without a GPU unit or while the board is busy.
    pub fn gpu_report(&self) -> Option<String> {
        let g = self.locks.quiesce(std::time::Duration::from_secs(5))?;
        g.raster.run_now(|ctx| ctx.gpu.as_ref().map(|u| u.report()))
    }

    /// The buffers the scan-out shows, one bit per window id.
    pub fn displayed_buffer(&self) -> u32 {
        self.locks.front.lock().displayed_buffer
    }

    pub fn shared(&self) -> &Arc<DisplayShared> {
        &self.locks.shared
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.mode == Mode::Threaded {
            self.locks.work_stop();
        }
    }
}
