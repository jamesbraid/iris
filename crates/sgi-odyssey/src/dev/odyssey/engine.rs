//! The VPro engine behind `BoardEngine`: aperture accesses in, events and
//! frames out, time and memory passed in, no emulator behind it.
//!
//! Everything below the register file is iris's own board code, mounted
//! unchanged; this file is the handle around it. The call contract is in the
//! `sgi_engine` crate docs. What is VPro's own:
//!
//! - The board is state plus locks (`BoardLocks`) that its threads and the
//!   caller share. The trait's `&mut self` only makes the shell serialise its
//!   calls, which it does anyway; each method reaches the board through
//!   `&BoardLocks`.
//! - `Mode::Inline` runs every store, draw and DMA on the caller's thread
//!   inside `write`, so `run` has nothing to do. `Mode::Threaded` runs the
//!   front end and the raster unit on threads of their own behind the stores.
//! - Interrupts are crosstalk packets (set and clear for the flag source, a
//!   set for retrace, DMA and fence), never a level the shell reads, so
//!   `irq_levels` is 0. A packet goes to the outbox from whichever thread
//!   raised it; the outbox keeps the newest per line.
//! - A fence's write-back is done by the raster unit through `ram_write`
//!   before it raises the fence's interrupt, so the guest cannot take the
//!   interrupt and read a stale fence.
//! - Every command-FIFO store is charged to the source `set_source` named and
//!   returned as `Event::Credits` by the pump once the board has retired it.
//! - The stream never waits on the shell: the board times its own retrace
//!   from `advance`, so `Idle` is `Yes`, `Working` or `Faulted`.
//! - A panic on the board's own threads (the front end, the raster unit and
//!   its pool) is caught and is the engine's fault, as the GPU unit's failure
//!   is. The fault wakes the shell at once, and the stores still queued are
//!   abandoned so `quiesce` ends and `reset` recovers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sgi_engine::inject::{hit, Point};
use sgi_engine::{GpuLoss, BoardEngine, Chunk, DirtyRun, Event, Fault, HostMem, FrameWords, Idle, Interrupt, Lent, LentFrame, MemFault, Outbox, Pump, Read, Rect, Refused, Source, StateSink, StateSource, Swap, Write, DIRTY_BLOCK};

use super::board::{Board, Params};
use super::gpu_unit::{fail_gpu, Observation};
use super::consts::{reset_pulse, ODSY_RESET_CONTROL};
use super::mmio::{access_waits, command_store, fifo_port, level_word, BoardGuard, BoardLocks};
use super::reader::{DisplayShared, ScanoutReader};
use super::state::ODYSSEY_SNAPSHOT_VERSION;
use super::vram::{VramPtr, DIRTY_BLOCK_SHIFT};
use crate::traits::{BusDevice, BusRead8, BUS_ERR, BUS_OK};

const _: () = assert!(DIRTY_BLOCK == 1 << DIRTY_BLOCK_SHIFT);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No engine threads: work runs inside the call that queues it.
    Inline,
    /// The board's front-end and raster threads run behind the stores.
    Threaded,
}

pub struct Config {
    /// `v12`, `v10`, `v8` or `v6`; empty uses `params` as given.
    pub product: String,
    /// The widget number the board answers as (its identity register).
    pub widget: u32,
    pub mode: Mode,
    /// The board properties; a product overrides some (`Params`).
    pub params: Params,
    /// The CPU raster unit's workers and tile size, as iris's `[graphics]`.
    pub pool: (usize, u32),
    /// Draw on the GPU through this wgpu backend name ("auto", "vulkan"...).
    pub gpu: Option<String>,
}

/// Board memory the product carries, in bytes, for a shell that lends it.
pub fn memory_bytes(product: &str) -> Result<u64, String> {
    memory_bytes_for(product, &Params::default())
}

/// Board memory of `product` (or of the properties `params` give, with no product), in bytes.
pub fn memory_bytes_for(product: &str, params: &Params) -> Result<u64, String> {
    let mut b = Box::new(Board::default());
    b.widget.revision = params.revision;
    b.widget.memory_size = params.memory_size;
    b.widget.memory_banks = params.memory_banks;
    b.widget.cas_latency = params.cas_latency;
    b.widget.backend_version = params.backend_version;
    b.widget.product = product.to_string();
    b.configure()?;
    Ok((b.widget.memory_size.max(32) as u64) << 20)
}

/// Guest memory as the board's DMA sees it: big-endian qwords over the shell's
/// `ram_*` path, which the board's threads may call. An access that reaches no
/// RAM answers a bus error and is reported once a page.
struct Guest<M> {
    mem: Arc<M>,
    out: Arc<Outbox>,
}

/// Words moved per call to the shell's memory.
const BLOCK_WORDS: usize = 512;

impl<M: HostMem> Guest<M> {
    fn fail(&self, addr: u64, write: bool) -> u32 {
        self.out.post(Event::BusFault { addr, write });
        BUS_ERR
    }
}

impl<M: HostMem> BusDevice for Guest<M> {
    fn read8(&self, addr: u64) -> BusRead8 {
        let mut b = [0u8];
        match self.mem.ram_read(addr, &mut b, Swap::None) {
            Ok(()) => BusRead8::ok(b[0]),
            Err(MemFault) => {
                self.fail(addr, false);
                BusRead8::err()
            }
        }
    }

    fn write8(&self, addr: u64, val: u8) -> u32 {
        match self.mem.ram_write(addr, &[val], Swap::None) {
            Ok(()) => BUS_OK,
            Err(MemFault) => self.fail(addr, true),
        }
    }

    fn write16(&self, addr: u64, val: u16) -> u32 {
        match self.mem.ram_write(addr, &val.to_be_bytes(), Swap::None) {
            Ok(()) => BUS_OK,
            Err(MemFault) => self.fail(addr, true),
        }
    }

    fn write32(&self, addr: u64, val: u32) -> u32 {
        match self.mem.ram_write(addr, &val.to_be_bytes(), Swap::None) {
            Ok(()) => BUS_OK,
            Err(MemFault) => self.fail(addr, true),
        }
    }

    fn read_block(&self, addr: u64, buf: &mut [u64]) -> u32 {
        let mut bytes = [0u8; BLOCK_WORDS * 8];
        for (i, words) in buf.chunks_mut(BLOCK_WORDS).enumerate() {
            let at = addr + (i * BLOCK_WORDS * 8) as u64;
            let bytes = &mut bytes[..words.len() * 8];
            if self.mem.ram_read(at, bytes, Swap::None).is_err() {
                return self.fail(at, false);
            }
            for (w, b) in words.iter_mut().zip(bytes.chunks_exact(8)) {
                *w = u64::from_be_bytes(b.try_into().unwrap());
            }
        }
        BUS_OK
    }

    fn write_block(&self, addr: u64, buf: &[u64]) -> u32 {
        let mut bytes = [0u8; BLOCK_WORDS * 8];
        for (i, words) in buf.chunks(BLOCK_WORDS).enumerate() {
            let at = addr + (i * BLOCK_WORDS * 8) as u64;
            let bytes = &mut bytes[..words.len() * 8];
            for (b, w) in bytes.chunks_exact_mut(8).zip(words) {
                b.copy_from_slice(&w.to_be_bytes());
            }
            if self.mem.ram_write(at, bytes, Swap::None).is_err() {
                return self.fail(at, true);
            }
        }
        BUS_OK
    }
}

/// Command-FIFO stores accepted and the sources they are charged to, in
/// order, so the credits a retired store frees go back to its source.
#[derive(Default)]
struct Credits {
    accepted: u64,
    returned: u64,
    runs: VecDeque<(Source, u64)>,
}

impl Credits {
    fn store(&mut self, src: Source) {
        self.accepted += 1;
        match self.runs.back_mut() {
            Some((s, c)) if *s == src => *c += 1,
            _ => self.runs.push_back((src, 1)),
        }
    }

    /// Of the `retired` stores accepted so far, hand each source the count
    /// since the last call, oldest first, for those that took a credit.
    fn retire(&mut self, retired: u64, mut give: impl FnMut(u32, u32)) {
        let mut n = retired.saturating_sub(self.returned);
        self.returned += n;
        while n > 0 {
            let Some((src, c)) = self.runs.front_mut() else { break };
            let take = n.min(*c);
            if src.charged {
                give(src.id, take as u32);
            }
            *c -= take;
            n -= take;
            if *c == 0 {
                self.runs.pop_front();
            }
        }
    }
}

/// How long one wait on the board lasts before the engine looks for a fault.
const SLICE: Duration = Duration::from_millis(50);
/// A budget no host clock overflows on (one year, in ns).
const FOREVER_NS: u64 = 31_536_000_000_000_000;
/// Bytes of board memory a `vram` chunk holds.
const PIECE_BYTES: u64 = 64 << 10;

/// The VPro engine.
pub struct VproEngine {
    locks: BoardLocks,
    out: Arc<Outbox>,
    /// The display's side of the scan-out: retrace clock and composed frame.
    reader: ScanoutReader,
    mode: Mode,
    gpu: bool,
    /// The size of the board memory the shell lent, if it did.
    lent: Option<u64>,
    vram: VramPtr,
    /// The board's threads are stopped (`quiesce`) until `resume`.
    parked: bool,
    source: Source,
    credits: Credits,
    failed: OnceLock<String>,
    fault_sent: Arc<AtomicBool>,
    /// Moves with every store, push, CPU write, load and reset: what the picture may follow
    /// (`frame_generation`, with the retrace swaps).
    activity: u64,
}

impl VproEngine {
    /// A board of `config.product`, reset to power-on. `lent` is the region
    /// the shell lends for board memory (`memory_bytes` long) or None for one
    /// the engine allocates; `mem` is the guest memory the board's DMA
    /// reaches, from the board's threads, so `ram_*` must be lock-free.
    /// `notify` only schedules the shell's pump.
    pub fn new<M: HostMem>(config: Config, lent: Option<Lent>, mem: M, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Result<Self, String> {
        let loss = GpuLoss::new();
        #[cfg(feature = "wgpu")]
        let gpu = match &config.gpu {
            Some(backend) => Some(crate::wgpu::Gpu::new(&crate::wgpu::GpuOptions { backend: backend.clone(), ..Default::default() }, loss.callback())?),
            None => None,
        };
        #[cfg(not(feature = "wgpu"))]
        let gpu = None;
        Self::with_gpu(config, lent, mem, notify, gpu, loss)
    }

    /// As `new`, with the GPU unit the caller opened (`config.gpu` is not read). `loss` is the
    /// target of that unit's device-loss callback (`GpuLoss::callback`).
    pub fn with_gpu<M: HostMem>(config: Config, lent: Option<Lent>, mem: M, notify: Option<Box<dyn Fn() + Send + Sync>>, gpu: Option<Arc<crate::wgpu::Gpu>>, loss: Arc<GpuLoss>) -> Result<Self, String> {
        let region = lent.map(|l| (l.as_ptr(), l.len()));
        // SAFETY: `Lent`'s contract: aligned, valid until the engine drops,
        // and touched by nothing but the engine and the shell's `cpu_wrote` protocol.
        let mut b = unsafe { Board::new_params(&config.product, config.widget, &config.params, region)? };
        b.raster.set_pool(config.pool.0, config.pool.1);
        let has_gpu = gpu.is_some();
        if let Some(gpu) = &gpu {
            #[cfg(feature = "wgpu")]
            b.enable_gpu(gpu)?;
            #[cfg(not(feature = "wgpu"))]
            return Err(format!("the GPU unit {:?} needs the wgpu feature", gpu.caps.adapter));
        }
        let out = Arc::new(Outbox::new(notify));
        b.guest = Some(Arc::new(Guest { mem: Arc::new(mem), out: out.clone() }));
        let sink = out.clone();
        let sender = config.widget as u8;
        b.front.lock().packets = Some(Arc::new(move |widget, addr, vector, level| {
            sink.post(Event::Interrupt(Interrupt { sender, widget, addr, vector, level }));
            true
        }));
        // Retired stores free credits, which only the pump returns.
        let wake = out.clone();
        b.fifo.on_retire(Box::new(move || {
            hit(Point::Exec);
            wake.wake()
        }));
        // A fault found on any thread tells the shell then, not at its next pump.
        let fault_sent = Arc::new(AtomicBool::new(false));
        let (told, sent) = (out.clone(), fault_sent.clone());
        b.gpu_fault.on_fault(Box::new(move |why| {
            if !sent.swap(true, Ordering::AcqRel) {
                told.post(Event::DeviceFault(why.to_string()));
            }
        }));
        b.reset();
        let (cell, told) = (b.gpu_fault.clone(), out.clone());
        loss.attach(move |why| {
            told.post(Event::GpuLost(why.to_string()));
            fail_gpu(&cell, "the GPU", why);
        });
        let vram = b.vram.ptr_shared();
        let locks = BoardLocks::new(b);
        let mut reader = ScanoutReader::default();
        reader.attach(&locks.shared);
        let e = VproEngine {
            locks,
            out,
            reader,
            mode: config.mode,
            gpu: has_gpu,
            lent: region.map(|r| r.1),
            vram,
            parked: false,
            source: Source::default(),
            credits: Credits::default(),
            failed: OnceLock::new(),
            fault_sent,
            activity: 0,
        };
        if config.mode == Mode::Threaded {
            e.locks.work_start();
        }
        Ok(e)
    }

    fn faulted(&self) -> bool {
        self.failed.get().is_some() || self.locks.shared.gpu_fault.lock().is_some()
    }

    fn size_of(bits: u32) -> Option<u32> {
        matches!(bits, 8 | 16 | 32 | 64).then_some(bits / 8)
    }

    /// Nothing runs on the board's threads and nothing waits for them.
    fn quiescent(&self) -> bool {
        let l = &self.locks;
        !l.fifo.running.load(Ordering::SeqCst) && !l.fifo.pending() && l.raster.pending() == 0
    }

    fn idle(&self) -> Idle {
        let l = &self.locks;
        if self.faulted() {
            Idle::Faulted
        } else if l.fifo.pending() || l.raster.pending() != 0 || l.fifo.slice.load(Ordering::SeqCst) {
            Idle::Working
        } else {
            Idle::Yes
        }
    }

    /// The board with its threads idle and the processors held off, waiting
    /// for the board's own work however long that takes: it waits on nothing
    /// the shell gives. Fails once the engine has stopped.
    fn settle(&self) -> Result<BoardGuard<'_>, Fault> {
        loop {
            if let Some(f) = self.fault() {
                return Err(Fault(f));
            }
            if let Some(g) = self.locks.quiesce(SLICE) {
                return Ok(g);
            }
        }
    }

    fn park(&mut self) {
        if self.mode == Mode::Threaded && !self.parked {
            self.locks.work_stop();
            self.parked = true;
        }
    }

    fn need_quiescent(&self) -> Result<(), Refused> {
        if let Some(f) = self.fault() {
            return Err(Refused::Faulted(f));
        }
        self.quiescent().then_some(()).ok_or(Refused::NotQuiesced)
    }

    /// Test hook, never reached by a guest: the board's thread holds its next slice open for up to `ms`
    /// milliseconds or until `release_hold`, so stores queue behind it and a read that waits answers busy.
    /// Threaded mode only; the call says so.
    #[cfg(feature = "test-hold")]
    pub fn hold_next_slice(&self, ms: u32) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        let fifo = &self.locks.fifo;
        fifo.test_release.store(false, SeqCst);
        fifo.hold_ms.store(ms, SeqCst);
        self.mode == Mode::Threaded
    }

    /// Ends a `hold_next_slice` early.
    #[cfg(feature = "test-hold")]
    pub fn release_hold(&self) {
        self.locks.fifo.test_release.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the board's thread is inside a slice now.
    #[cfg(feature = "test-hold")]
    pub fn slice_open(&self) -> bool {
        self.locks.fifo.slice.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The cursor's position and whether it is drawn, from the display registers.
    pub fn cursor(&self) -> (i32, i32, bool) {
        self.locks.shared.regs.cursor_state()
    }

    /// The GPU unit's records, batches, copies, readbacks, rejections and faults; None without a
    /// GPU unit or once the engine has stopped.
    pub fn gpu_counters(&self) -> Option<[u64; 6]> {
        let g = self.settle().ok()?;
        g.raster.run_now(|ctx| ctx.gpu.as_ref().map(|u| u.counters()))
    }

    /// The GPU unit's counters (batches, records, waits, readbacks) as
    /// text; None without a GPU unit or once the engine has stopped.
    pub fn gpu_report(&self) -> Option<String> {
        let g = self.settle().ok()?;
        g.raster.run_now(|ctx| ctx.gpu.as_ref().map(|u| u.report()))
    }

    /// The buffers the scan-out shows, one bit per window id.
    pub fn displayed_buffer(&self) -> u32 {
        self.locks.front.lock().displayed_buffer
    }

    pub fn shared(&self) -> &Arc<DisplayShared> {
        &self.locks.shared
    }

    /// The scan-out composed from board memory as the registers describe
    /// it, 0x00RRGGBB, `(width, height, pixels)`; None with no mode or once
    /// the engine has stopped. With the GPU scan-out it composes on the GPU
    /// behind the work already submitted and reads the frame back: it waits for
    /// the GPU, not for the board's queued CPU work. Without it nothing
    /// waits: the frame is composed from memory the raster threads may still be
    /// writing, as the board's own display thread does, so a drawing in progress
    /// shows for one frame.
    pub fn scanout(&mut self) -> Option<(u32, u32, Vec<u32>)> {
        if self.faulted() {
            return None;
        }
        // The GPU scan-out composes from the unit's own memory, in order behind the work submitted so
        // far, so it waits for the GPU and not for the board's queued CPU work. The unit takes the
        // CPU's stores (`cpu_wrote`) and submits its open batch at a tick, which needs the board: done
        // when the board is idle now, skipped (not waited for) while it runs a slice. Without a GPU
        // scan-out (the GPU cannot hold the frame) the CPU compose reads the mirror, which the
        // readback brings current.
        if self.gpu {
            if self.locks.shared.gpu_scanout.get().is_none() {
                let g = self.settle().ok()?;
                g.gpu_upload_tick(true);
                g.gpu_sync();
            } else if let Some(g) = self.locks.quiesce(Duration::ZERO) {
                g.gpu_upload_tick(true);
            }
        }
        let r = &mut self.reader;
        r.attach(&self.locks.shared);
        r.recompose();
        r.update_to(None);
        // The reader composes 0xAABBGGRR; this API hands out 0x00RRGGBB.
        let rgb = |p: &u32| (p & 0xff) << 16 | (p & 0xff00) | (p >> 16) & 0xff;
        (r.frame_width != 0).then(|| (r.frame_width, r.frame_height, r.frame.iter().map(rgb).collect()))
    }
}

impl BoardEngine for VproEngine {
    /// A box of pixels; every plane under it is brought current.
    type Region = Rect;

    fn heads(&self) -> u32 {
        1
    }

    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        out.clear();
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        if head != 0 {
            return Ok(None);
        }
        Ok(VproEngine::scanout(self).map(|(w, h, pixels)| {
            *out = pixels;
            (w, h)
        }))
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        let size = self.locks.shared.mode_size();
        (head == 0 && size.0 != 0 && size.1 != 0).then_some(size)
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if head != 0 {
            return 0;
        }
        // Every term only grows, so the sum moves when any does; never 0.
        1 + self.activity + self.locks.shared.swaps.load(Ordering::Relaxed)
    }

    /// The composed frame left in the GPU scan-out's buffer, no readback; None without the GPU unit
    /// or while there is no video mode. Its words are 0xAABBGGRR (`FrameWords::Abgr`).
    #[cfg(feature = "wgpu")]
    fn lend_frame(&mut self, head: u32) -> Result<Option<LentFrame>, Fault> {
        if head != 0 || !self.gpu {
            return Ok(None);
        }
        {
            let g = self.settle()?;
            g.gpu_upload_tick(true);
        }
        let generation = self.frame_generation(0);
        let r = &mut self.reader;
        r.attach(&self.locks.shared);
        Ok(r.update_on_gpu().map(|(buffer, width, height)| LentFrame { width, height, stride: width, words: FrameWords::Abgr, generation, buffer: Arc::new(buffer) }))
    }

    /// The frame composed into `dst` (a `wgpu::Buffer`, or an `Arc` of one, on the engine's device) as
    /// 0x00RRGGBB words, `stride_words` a row.
    #[cfg(feature = "wgpu")]
    fn compose_into(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if head != 0 || !self.gpu {
            return Ok(None);
        }
        let dst = match (dst.downcast_ref::<wgpu::Buffer>(), dst.downcast_ref::<Arc<wgpu::Buffer>>()) {
            (Some(b), _) => b,
            (None, Some(b)) => b.as_ref(),
            (None, None) => return Err(Fault("compose_into: the destination is not a wgpu buffer".into())),
        };
        // The destination is checked against the mode's size, which no frame exceeds, before the board
        // or the device is touched: an error of the call, not a fault.
        if let Some((w, h)) = BoardEngine::frame_size(self, 0) {
            crate::wgpu::Scanout::check_into(w, h, dst, stride_words).map_err(|why| Fault(format!("compose_into: {why}")))?;
        }
        {
            let g = self.settle()?;
            g.gpu_upload_tick(true);
        }
        let r = &mut self.reader;
        r.attach(&self.locks.shared);
        // A refused destination is an error of the call; a fault after it is the device's.
        let composed = r.update_into_gpu(dst, stride_words).map_err(|why| Fault(format!("compose_into: {why}")));
        if composed.is_ok() {
            if let Some(f) = self.fault() {
                return Err(Fault(f));
            }
        }
        composed
    }

    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        // A store the board takes inside this call (every one in inline mode) restarts the retrace at `now_ns`.
        let shared = self.locks.shared.clone();
        shared.write_ns.store(now_ns.min(i64::MAX as u64) as i64, Ordering::Relaxed);
        let done = self.write(off, bits, val);
        shared.write_ns.store(-1, Ordering::Relaxed);
        done
    }

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        self.activity += 1;
        let Some(size) = Self::size_of(bits) else { return Write::Fault };
        if self.faulted() {
            return Write::Fault;
        }
        if !self.locks.bus_write(off as u64, val, size) {
            return Write::Busy;
        }
        if command_store(off as u64, size) {
            self.credits.store(self.source);
        }
        if off as u64 == ODSY_RESET_CONTROL && reset_pulse(val) {
            // The board reset itself; levels may have changed. The store waited for the FIFO to
            // drain, so credits still owed are for retired stores and go back with the next pump.
            self.out.post(Event::Irq);
        }
        Write::Done
    }

    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        self.activity += 1;
        if !fifo_port(port as u64) {
            return 0;
        }
        words.iter().take_while(|&&w| self.write(port, 32, w as u64) == Write::Done).count()
    }

    fn read(&mut self, off: u32, bits: u32) -> Read {
        let Some(size) = Self::size_of(bits) else { return Read::Fault };
        if self.faulted() {
            return Read::Fault;
        }
        match self.locks.bus_read(off as u64, size) {
            Some(_) if self.faulted() => Read::Fault,
            Some(v) => Read::Value(v),
            None => Read::Busy,
        }
    }

    fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        let Some(size) = Self::size_of(bits) else { return false };
        if self.faulted() {
            return false;
        }
        let (a, l) = (off as u64, &self.locks);
        if write && command_store(a, size) {
            return l.fifo.full_for(size);
        }
        if !access_waits(a, size, write) || (!write && level_word(a, size)) || !l.fifo.running.load(Ordering::SeqCst) {
            return false;
        }
        l.fifo.pending() || (!write && l.raster.pending() != 0)
    }

    fn run(&mut self, _max_ops: usize) -> usize {
        // Inline, a store runs in the call that made it, so nothing waits.
        0
    }

    fn advance(&mut self, now_ns: u64) -> Pump {
        if !self.locks.fifo.running.load(Ordering::SeqCst) {
            // The GPU unit sends the mirror's changes up and submits its open
            // batch at the end of a run of stores; with no board thread to end
            // one, this call does.
            // SAFETY: no board thread runs, and the caller is the only thread using the board.
            unsafe { &*self.locks.board.get() }.gpu_upload_tick(true);
        }
        let r = &mut self.reader;
        r.attach(&self.locks.shared);
        r.poll(now_ns.min(i64::MAX as u64) as i64);
        let next_deadline = (r.clock.deadline >= 0).then_some(r.clock.deadline as u64);
        let retired = self.credits.accepted.saturating_sub(self.locks.fifo.stores() as u64);
        let out = &self.out;
        self.credits.retire(retired, |source, retired| out.post(Event::Credits { source, retired }));
        out.rearm();
        Pump { idle: self.idle(), events: out.pending(), next_deadline, irq_levels: 0 }
    }

    fn wait_idle(&mut self, _now_ns: u64, budget_ns: u64) -> Idle {
        if self.faulted() {
            return Idle::Faulted;
        }
        let done = self.locks.quiesce(Duration::from_nanos(budget_ns.min(FOREVER_NS))).is_some();
        match self.idle() {
            Idle::Faulted => Idle::Faulted,
            _ if done => Idle::Yes,
            _ => Idle::Working,
        }
    }

    fn irq_levels(&self) -> u64 {
        0
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        if let Some(f) = self.fault() {
            if !self.fault_sent.swap(true, Ordering::AcqRel) {
                self.out.post(Event::DeviceFault(f));
            }
        }
        self.out.drain(out, max);
    }

    fn drain_dirty(&mut self, out: &mut Vec<DirtyRun>) {
        let running = self.locks.fifo.running.load(Ordering::SeqCst);
        out.extend(self.vram.take_reported(running).into_iter().map(|(offset, len)| DirtyRun { offset, len }));
    }

    fn trapped(&self, out: &mut Vec<DirtyRun>) {
        // With the GPU unit the memory holds what the unit last read back.
        if let (true, Some(len)) = (self.gpu, self.lent) {
            out.push(DirtyRun { offset: 0, len });
        }
    }

    fn observe_lent(&mut self, _off: u64, _len: u64) -> Result<(), Fault> {
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        if !self.gpu || self.lent.is_none() {
            return Ok(());
        }
        let g = self.settle()?;
        g.gpu_sync_result(Observation::Frontend).map_err(Fault)
    }

    fn cpu_wrote(&mut self, off: u64, len: u64) {
        self.activity += 1;
        self.vram.mark_for_upload(off, len);
    }

    fn observe(&mut self, r: &Rect) -> Result<(), Fault> {
        let g = self.settle()?;
        let _ = g.planes_in(r.x0 as i64, r.y0 as i64, r.x1 as i64, r.y1 as i64);
        Ok(())
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_quiescent()?;
        let g = self.settle().map_err(|f| Refused::Faulted(f.0))?;
        g.gpu_upload_tick(true);
        g.gpu_sync_result(Observation::Validation).map_err(Refused::Faulted)
    }

    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        if !self.faulted() && self.locks.quiesce(Duration::from_nanos(budget_ns.min(FOREVER_NS))).is_none() && !self.faulted() {
            return Err(Refused::Midway("the board was still drawing at the budget".into()));
        }
        self.park();
        self.fault().map_or(Ok(()), |f| Err(Refused::Faulted(f)))
    }

    fn resume(&mut self) {
        if self.parked {
            self.parked = false;
            self.locks.work_start();
        }
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_quiescent()?;
        let put = |sink: &mut dyn StateSink, name: &str, index: u64, bytes: Vec<u8>| sink.put(Chunk { name: name.into(), index, bytes }).map_err(|e| Refused::Faulted(e.0));
        let (meta, state) = self.locks.snapshot_state(self.lent.is_some()).map_err(|e| self.fault().map_or_else(|| Refused::Midway(e), Refused::Faulted))?;
        put(sink, "meta", 0, meta.into_bytes())?;
        put(sink, "state", 0, state)?;
        if self.lent.is_none() {
            // The engine's own memory, a piece a chunk, the zero pieces left out.
            let size = self.vram.size();
            for i in 0..size.div_ceil(PIECE_BYTES) {
                let words = self.vram.words_at(i * PIECE_BYTES, PIECE_BYTES.min(size - i * PIECE_BYTES));
                if words.iter().any(|&w| w != 0) {
                    put(sink, "vram", i, words.iter().flat_map(|w| w.to_le_bytes()).collect())?;
                }
            }
        }
        Ok(())
    }

    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        self.activity += 1;
        self.need_quiescent()?;
        let bad = |m: String| Refused::Mismatch(m);
        let lent = self.lent.is_some();
        let (mut meta, mut state) = (false, None);
        while let Some(c) = src.next().map_err(|e| bad(e.0))? {
            match c.name.as_str() {
                "meta" if !meta => {
                    let text = String::from_utf8(c.bytes).map_err(|e| bad(e.to_string()))?;
                    self.locks.check_metadata(&text, lent).map_err(bad)?;
                    meta = true;
                    if !lent {
                        // The pieces that are all zero are not in the stream.
                        for off in (0..self.vram.size()).step_by(PIECE_BYTES as usize) {
                            self.vram.write_words_untracked(off, &vec![0u64; (PIECE_BYTES.min(self.vram.size() - off) / 8) as usize]);
                        }
                    }
                }
                _ if !meta => return Err(bad(format!("the state's first chunk is {}, not meta", c.name))),
                "state" if state.is_none() => state = Some(c.bytes),
                "vram" if !lent => {
                    let off = c.index.saturating_mul(PIECE_BYTES);
                    if c.bytes.len() % 8 != 0 || off.saturating_add(c.bytes.len() as u64) > self.vram.size() {
                        return Err(bad(format!("memory piece {} has {} bytes", c.index, c.bytes.len())));
                    }
                    let words: Vec<u64> = c.bytes.chunks_exact(8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).collect();
                    self.vram.write_words_untracked(off, &words);
                }
                other => return Err(bad(format!("unexpected state chunk {other}"))),
            }
        }
        let Some(state) = state else { return Err(bad("the state has no board state".into())) };
        // The memory is whole before the state that reads it (the GPU unit
        // reseeds from it).
        let vram = self.vram;
        self.locks.restore_state(&state, |_| vram.mark_written()).map_err(bad)?;
        self.credits = Credits::default();
        self.out.post(Event::Irq);
        Ok(())
    }

    fn state_version(&self) -> u32 {
        ODYSSEY_SNAPSHOT_VERSION
    }

    fn reset(&mut self) -> Result<(), Refused> {
        self.activity += 1;
        if !self.quiescent() {
            return Err(Refused::NotQuiesced);
        }
        self.locks.with_stopped_board(|b| b.reset());
        self.out.clear();
        // The stores the reset abandoned take their credits with them, as the
        // events that would have returned them.
        self.credits = Credits::default();
        self.failed.take();
        self.fault_sent.store(false, Ordering::Release);
        self.out.post(Event::Irq);
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.failed.get().cloned().or_else(|| self.locks.gpu_fault())
    }

    fn fail(&self, why: &str) {
        let _ = self.failed.set(why.to_string());
    }
}

impl Drop for VproEngine {
    fn drop(&mut self) {
        if self.mode == Mode::Threaded {
            self.locks.work_stop();
        }
    }
}

