//! GR2 board engine: the register window, the HQ2 and RE3 threads, and the
//! finish-flag, read-port and context-save handshakes the host sees.
//!
//! What it leaves to its shell: the bus (widths, byte order, retry), the
//! retrace and the display thread, the monitor commands and saved state. The
//! shell gives it the time (`set_time`) and the board's probe answers
//! (`Gr2Probe`). Moved out of `mod.rs`, which keeps iris's board around it.

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;

use crate::gfifo::GFifo;
use crate::traits::{BusRead32, BUS_BUSY, BUS_OK};

use super::bt457::Bt457;
use super::debug;
use super::ge7::Ge7;
use super::hq2::{self, Hq2Engine, Hq2Regs, Re3Sink};
#[cfg(feature = "gr2-jit")]
use super::re3_jit;
use super::re3::{self, Re3};
#[cfg(feature = "wgpu")]
use super::scanout;
use super::vc1::Vc1;
use super::xmap5::{self, Xmap5};

/// GIO gfx slot base and size (the board decodes a 4 MB slot).
pub const GR2_BASE: u64 = 0x1F00_0000;
pub const GR2_SLOT_SIZE: u32 = 0x0040_0000;

/// Bus address to a 32-bit offset from the slot base. The decode table only
/// routes the GR2 slot here, so the truncation never loses bits.
#[inline]
pub(super) fn slot_off(addr: u64) -> u32 {
    (addr - GR2_BASE) as u32
}

// Region offsets from the board base: the gr2 survey's host contract.
pub(super) const SHRAM: u32 = 0x00000;
pub(super) const SHRAM_END: u32 = 0x20000;
pub(super) const FIFO: u32 = 0x40000;
pub(super) const FIFO_END: u32 = 0x60000;
pub(super) const HQUCODE: u32 = 0x60000;
pub(super) const HQUCODE_END: u32 = 0x68000;
pub(super) const GEWIN: u32 = 0x68000;
pub(super) const GEWIN_END: u32 = 0x6a000;
pub(super) const HQREGS: u32 = 0x6a000;
pub(super) const HQREGS_END: u32 = 0x6a104;
pub(super) const BDVERS: u32 = 0x6c000;
pub(super) const CLOCK: u32 = 0x6c020;
pub(super) const VC1: u32 = 0x6c040;
pub(super) const DAC: u32 = 0x6c0a0;
pub(super) const XMAP: u32 = 0x6c100;
pub(super) const XMAPALL: u32 = 0x6c1a0;
pub(super) const AB1: u32 = 0x6c1c0;
pub(super) const CC1: u32 = 0x6c1e0;
pub(super) const RE3_REGS: u32 = 0x6c200;
pub(super) const RE3_REGS_END: u32 = 0x6c300;
pub(super) const RE3_32: u32 = 0x6c600;

pub const SHRAM_WORDS: usize = 32768;
/// shram word the PROM sets to 1 (UCODE_TP) once the textport microcode runs.
pub const TP_PROBE_ID: usize = 0x7fff;

pub const HQ_FIFO_DEPTH: usize = 65536;
pub const RE3_FIFO_DEPTH: usize = 65536;
/// Words the HQ2_GEDMA read port holds ahead of its reader (power of two).
pub const GEDMA_OUT_WORDS: usize = 16384;
/// How long the HQ2 waits for a reader when the read port is full.
pub(super) const GEDMA_OUT_STALL_LIMIT_NS: u64 = 2_000_000_000;

/// What the board answers a probe with. The shell sets it: it is the
/// revision and wiring of the machine the board sits in, not the engine's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gr2Probe {
    /// Geometry engines present: 1, 2, 4 or 8.
    pub ges: u32,
    /// What the four board configuration ports (+0x6c000 to +0x6c00c) read as.
    pub ports: [u32; 4],
    /// The HQ2 revision in the status word and `hq_gepc`.
    pub hq_rev: u32,
}

/// CPU-side register and memory state. Plain data, zero-valid.
#[repr(C)]
pub struct Gr2Regs {
    pub shram: [u32; SHRAM_WORDS],
    pub hq: Hq2Regs,
    pub ge: Ge7,
    /// Last configuration writes to rd0/rd1 (debug only; reads return IDs).
    pub bdvers_w: [u32; 4],
    pub vc1: Vc1,
    pub dac: [Bt457; 3],
    pub xmap: [Xmap5; 5],
    pub ab1: u32,
    /// Pixel-clock load on the data port (revision 4 and up): bit 0 data, LSB
    /// first per byte; bit 1 set on the final bit. Bits collected so far,
    /// count, and the last complete 7-byte programming.
    pub pll_bits: u64,
    pub pll_count: u32,
    pub pll_last: [u8; 8],
    pub pll_programs: u32,
}

pub struct Gr2 {
    /// The board iris built, for snapshots; the probe answers come from `probe`.
    pub(super) variant: Gr2Variant,
    pub(super) probe: Gr2Probe,
    /// Perfetto edge trackers (zero-valid, so `new`'s zeroed allocation
    /// initialises them): a guest read stalled on the engines, a store
    /// stalled on a full FIFO.
    pub(super) pf_wait: crate::PfLevel,
    pub(super) pf_fifo_full: crate::PfLevel,
    pub(super) regs: UnsafeCell<Gr2Regs>,
    pub(super) re3: UnsafeCell<Re3>,
    pub(super) hq_engine: UnsafeCell<Hq2Engine>,
    /// Finish / 2D sync tokens (0x0A3, 0x155) the CPU has queued that the HQ2
    /// has not executed yet. FIN3 reads as clear while any is pending: with
    /// our deep FIFO an older Finish would otherwise raise FIN3 for a newer
    /// wait, and the pipeline would run a whole frame behind every swap
    /// (ideas flicker, IRIX 6.5.22). See clanker/gr2/fin3-must-track-pending-finish.md.
    pub(super) fin3_pending: AtomicU32,
    /// Set when the kernel acks FIN2 (write to 0x6A04C), which it does right
    /// before issuing a FIN2 command (pixel DMA 0x147, context save/restore)
    /// and polling version bit 1 in a counted loop (100,000 x us_delay(1) for
    /// pixel DMA). Our HQ2 may still be drawing a large pixel DMA when that
    /// budget runs out: IRIX 5.3 then logged "Gr2PixelDma: TIMEOUT", raised a
    /// graphics error and detached Xsgi. While set, a version read with FIN2
    /// clear stalls (bus busy) as long as the HQ2 still has work, so the
    /// kernel sees FIN2 as soon as the HQ2 gets there. See
    /// clanker/gr2/fin2-wait-must-stall.md.
    pub(super) fin2_wait: AtomicU32,
    /// Shell time (`now_ns`) of the last progress seen by a stalled FIN2
    /// poll since the wait was armed (0 = no stalled poll yet). The stall
    /// gives up after FIN2_STALL_LIMIT without progress, so a pipeline that
    /// can never finish (RE3 held by a CPU-driven readback) cannot hang the
    /// machine, while a long but moving backlog is waited out: gltest
    /// --bench 500 (500 full-screen quads, ~2.4 s of RE3 work) timed out a
    /// context switch's FIN2 with a fixed 2 s cap and crashed the kernel in
    /// Gr2PcxSwap. Not counted from the ack: the DMA between ack and poll
    /// may take long on a busy host (a 400-row pixel DMA on a GitHub CI
    /// runner did).
    pub(super) fin2_armed_ns: AtomicU64,
    /// The shell's clock in ns, which the two stall limits count in; the
    /// board reads no host clock. `set_time` moves it forward.
    pub(super) now_ns: AtomicU64,
    /// HQ2 + RE3 FIFO consumer positions at the last stalled FIN2 poll.
    pub(super) fin2_progress: AtomicU64,
    /// HQ2_GEDMA read port: HQ2 -> host words (context saves 0x1E1, pixel
    /// DMA reads 0x152 / 0x0AC). A ring the HQ2 thread fills and the reader
    /// (the kernel's VDMA) drains; head / tail count words since reset.
    /// A read with the ring empty waits (bus busy) while the HQ2 still has
    /// work, since the kernel starts the VDMA right after queuing the
    /// request. See clanker/gr2/gedma-read-port.md.
    pub(super) gedma_out: [AtomicU32; GEDMA_OUT_WORDS],
    pub(super) gedma_out_head: AtomicU32,
    pub(super) gedma_out_tail: AtomicU32,
    /// Shell time `gedma_push` began to wait for a reader (0: not waiting); it gives up a limit later.
    pub(super) gedma_stall_since: AtomicU64,
    pub(super) hq_fifo: GFifo<HQ_FIFO_DEPTH>,
    pub(super) re3_fifo: GFifo<RE3_FIFO_DEPTH>,
    /// Pending half of a two-entry COPY op (RE3 thread only).
    pub(super) copy_a: Cell<u64>,

    pub(super) running: AtomicBool,
    pub(super) hq_busy: AtomicBool,
    pub(super) re3_busy: AtomicBool,
    pub(super) threads: Mutex<Vec<thread::JoinHandle<()>>>,
    /// What an engine thread that panicked left: the shell's hook is told once.
    pub(super) panicked: Mutex<Option<String>>,
    /// Bit 0: the HQ2 thread ended in a panic; bit 1: the RE3 thread did.
    pub(super) dead: AtomicU32,
    pub(super) on_panic: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    pub(super) hq_thread: Mutex<Option<thread::Thread>>,
    pub(super) re3_thread: Mutex<Option<thread::Thread>>,
    /// The engine threads parked (`sleep`): producers unpark one only then,
    /// so a push reads a line nobody writes instead of taking the thread's
    /// slot lock and unparking it every time. Each on its own cache line.
    pub(super) hq_asleep: crossbeam_utils::CachePadded<AtomicBool>,
    /// Bumped by every CPU write to the RE3 registers: HQ2's register
    /// shadow (`hq_loop`'s Sink) no longer knows what RE3 holds then.
    pub(super) re3_cpu_writes: crossbeam_utils::CachePadded<AtomicU32>,
    pub(super) re3_asleep: crossbeam_utils::CachePadded<AtomicBool>,

    pub(super) published_raster: PublishedRe3Telemetry,
    pub(super) presentation_frames: AtomicU64,
    pub(super) presentation_host_read_pixels: AtomicU64,

    /// The display channel: screen, renderer slot and frame requests, run
    /// by the machine's refresh thread (`crate::display`).
    pub(super) head: Arc<crate::display::HeadHandle>,
    /// The scan-out on the GPU, when the machine opened one: the refresh
    /// thread's alone. Dropped there if it fails, and the CPU compose takes over.
    #[cfg(feature = "wgpu")]
    pub(super) scanout: UnsafeCell<Option<Box<scanout::Gr2Scanout>>>,
    pub(super) retrace_cb: Mutex<Option<Arc<dyn Fn(bool) + Send + Sync>>>,
    pub(super) status: Arc<crate::status::MachineStatus>,
    /// Annotated FIFO capture (`gr2 trace`); `trace_mask` gates the hot path.
    pub(super) trace: Mutex<debug::Gr2Trace>,
    pub(super) trace_mask: AtomicU32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gr2TelemetrySnapshot {
    pub raster: re3::Re3TelemetrySnapshot,
    pub presentation_frames: u64,
    pub presentation_host_read_pixels: u64,
    #[cfg(feature = "gr2-jit")]
    pub jit_hits: u64,
    #[cfg(feature = "gr2-jit")]
    pub jit_misses: u64,
}

pub(super) struct PublishedRe3Telemetry {
    spans: AtomicU64,
    pixels: AtomicU64,
    span_buckets: [AtomicU64; 8],
    batchable_spans: AtomicU64,
    max_batch: AtomicU32,
    sync_boundaries: AtomicU64,
    framebuffer_read_words: AtomicU64,
    framebuffer_write_words: AtomicU64,
    z_read_words: AtomicU64,
    z_write_words: AtomicU64,
    host_read_pixels: AtomicU64,
    #[cfg(feature = "gr2-jit")]
    jit_hits: AtomicU64,
    #[cfg(feature = "gr2-jit")]
    jit_misses: AtomicU64,
}

impl PublishedRe3Telemetry {
    pub(super) fn load(&self) -> re3::Re3TelemetrySnapshot {
        re3::Re3TelemetrySnapshot {
            spans: self.spans.load(Ordering::Relaxed),
            pixels: self.pixels.load(Ordering::Relaxed),
            span_buckets: std::array::from_fn(|i| self.span_buckets[i].load(Ordering::Relaxed)),
            batchable_spans: self.batchable_spans.load(Ordering::Relaxed),
            max_batch: self.max_batch.load(Ordering::Relaxed),
            sync_boundaries: self.sync_boundaries.load(Ordering::Relaxed),
            framebuffer_read_words: self.framebuffer_read_words.load(Ordering::Relaxed),
            framebuffer_write_words: self.framebuffer_write_words.load(Ordering::Relaxed),
            z_read_words: self.z_read_words.load(Ordering::Relaxed),
            z_write_words: self.z_write_words.load(Ordering::Relaxed),
            host_read_pixels: self.host_read_pixels.load(Ordering::Relaxed),
        }
    }

    pub(super) fn store(&self, value: re3::Re3TelemetrySnapshot) {
        self.spans.store(value.spans, Ordering::Relaxed);
        self.pixels.store(value.pixels, Ordering::Relaxed);
        for (slot, value) in self.span_buckets.iter().zip(value.span_buckets) {
            slot.store(value, Ordering::Relaxed);
        }
        self.batchable_spans.store(value.batchable_spans, Ordering::Relaxed);
        self.max_batch.store(value.max_batch, Ordering::Relaxed);
        self.sync_boundaries.store(value.sync_boundaries, Ordering::Relaxed);
        self.framebuffer_read_words.store(value.framebuffer_read_words, Ordering::Relaxed);
        self.framebuffer_write_words.store(value.framebuffer_write_words, Ordering::Relaxed);
        self.z_read_words.store(value.z_read_words, Ordering::Relaxed);
        self.z_write_words.store(value.z_write_words, Ordering::Relaxed);
        self.host_read_pixels.store(value.host_read_pixels, Ordering::Relaxed);
    }

    pub(super) fn reset(&self) {
        self.store(re3::Re3TelemetrySnapshot::default());
        #[cfg(feature = "gr2-jit")]
        {
            self.jit_hits.store(0, Ordering::Relaxed);
            self.jit_misses.store(0, Ordering::Relaxed);
        }
    }
}

// SAFETY: same model as Rex3. `regs` is written only by the CPU thread (plus
// monitor/snapshot with the machine stopped). `re3` and `hq_engine` are owned
// by their threads, and the CPU only touches them after both FIFOs are
// drained. The display thread reads with tearing tolerated. `copy_a` is RE3
// thread only. `cycles` is set once during single-threaded setup.
unsafe impl Sync for Gr2 {}
unsafe impl Send for Gr2 {}

impl Gr2 {
    /// Build a GR2 in place on the heap. The multi-megabyte register/VRAM
    /// state is zero-initialised without ever touching the stack; only the
    /// runtime fields that are not all-zero-valid are written explicitly.
    ///
    /// With `gpu` the frame is composed on the host GPU and a window on the
    /// same device shows it without a readback. The raster stays on the CPU.
    /// The machine opened the device and hands the board its own handle on
    /// it (`Gpu::share`).
    pub fn build(variant: Gr2Variant, probe: Gr2Probe, status: Arc<crate::status::MachineStatus>, gpu: Option<crate::SharedGpu>) -> Result<Arc<Self>, String> {
        #[cfg(feature = "wgpu")]
        let scanout = match gpu {
            Some(g) => {
                eprintln!("gr2: scan-out on the GPU: {}", g.caps);
                Some(Box::new(scanout::Gr2Scanout::new(&g)?))
            }
            None => None,
        };
        #[cfg(not(feature = "wgpu"))]
        let _ = gpu;
        let mut a: Arc<MaybeUninit<Self>> = Arc::new_zeroed();
        let p = Arc::get_mut(&mut a).unwrap().as_mut_ptr();
        // SAFETY: `p` points at zeroed, exclusively owned memory. Every field
        // that is not valid when zeroed is initialised below, before assume_init.
        unsafe {
            addr_of_mut!((*p).variant).write(variant);
            addr_of_mut!((*p).probe).write(probe);
            addr_of_mut!((*p).copy_a).write(Cell::new(0));
            addr_of_mut!((*p).running).write(AtomicBool::new(false));
            addr_of_mut!((*p).hq_busy).write(AtomicBool::new(false));
            addr_of_mut!((*p).re3_busy).write(AtomicBool::new(false));
            addr_of_mut!((*p).threads).write(Mutex::new(Vec::new()));
            addr_of_mut!((*p).panicked).write(Mutex::new(None));
            addr_of_mut!((*p).on_panic).write(Mutex::new(None));
            addr_of_mut!((*p).hq_thread).write(Mutex::new(None));
            addr_of_mut!((*p).re3_thread).write(Mutex::new(None));
            #[cfg(feature = "wgpu")]
            addr_of_mut!((*p).scanout).write(UnsafeCell::new(scanout));
            addr_of_mut!((*p).head).write(status.refresh().head(status.clone()));
            addr_of_mut!((*p).retrace_cb).write(Mutex::new(None));
            addr_of_mut!((*p).status).write(status);
            addr_of_mut!((*p).trace).write(Mutex::new(debug::Gr2Trace::new()));
            addr_of_mut!((*p).trace_mask).write(AtomicU32::new(0));
            #[cfg(feature = "gr2-jit")]
            addr_of_mut!((*(*p).re3.get()).jit.mode).write(re3_jit::default_mode());
            let a = a.assume_init();
            let head: Arc<dyn crate::display::Head> = a.clone();
            a.head.attach(Arc::downgrade(&head));
            a.power_on();
            Ok(a)
        }
    }


    pub fn variant(&self) -> Gr2Variant { self.variant }
    pub fn probe(&self) -> Gr2Probe { self.probe }

    pub fn telemetry_snapshot(&self) -> Gr2TelemetrySnapshot {
        Gr2TelemetrySnapshot {
            raster: self.published_raster.load(),
            presentation_frames: self.presentation_frames.load(Ordering::Relaxed),
            presentation_host_read_pixels: self.presentation_host_read_pixels.load(Ordering::Relaxed),
            #[cfg(feature = "gr2-jit")]
            jit_hits: self.published_raster.jit_hits.load(Ordering::Relaxed),
            #[cfg(feature = "gr2-jit")]
            jit_misses: self.published_raster.jit_misses.load(Ordering::Relaxed),
        }
    }

    pub(super) fn publish_re3_telemetry(&self, re3: &re3::Re3) {
        self.published_raster.store(re3.telemetry_snapshot());
        #[cfg(feature = "gr2-jit")]
        {
            self.published_raster.jit_hits.store(re3.jit.hits, Ordering::Relaxed);
            self.published_raster.jit_misses.store(re3.jit.misses, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    pub(super) fn unimplemented_commands(&self) -> Vec<u32> {
        // SAFETY: tests call this after wait_idle.
        unsafe { (&*self.hq_engine.get()).unimplemented_commands() }
    }

    pub fn set_retrace_callback(&self, cb: Arc<dyn Fn(bool) + Send + Sync>) {
        *self.retrace_cb.lock() = Some(cb);
    }

    #[allow(clippy::mut_from_ref)]
    pub(super) fn regs(&self) -> &mut Gr2Regs {
        // SAFETY: see the Sync impl.
        unsafe { &mut *self.regs.get() }
    }

    /// VRAM, for the display thread and tests.
    pub fn vram(&self) -> &[u32] {
        // SAFETY: read-only view; tearing tolerated.
        unsafe { &(*self.re3.get()).vram }
    }

    /// RE3 Z control (tests; call when idle).
    #[cfg(test)]
    pub fn re3_zctl(&self) -> u32 {
        // SAFETY: read-only, used after wait_idle.
        unsafe { (*self.re3.get()).ctx.zctl }
    }

    /// RE3 pixel format (tests; call when idle).
    #[cfg(test)]
    pub fn re3_ctx_pixfmt(&self) -> u32 {
        // SAFETY: read-only, used after wait_idle.
        unsafe { (*self.re3.get()).ctx.pixfmt }
    }

    /// RE3 register value (tests; call when idle).
    #[cfg(test)]
    pub fn re3_reg(&self, reg: usize) -> u32 {
        // SAFETY: read-only, used after wait_idle.
        unsafe { (*self.re3.get()).ctx.reg[reg & 63] }
    }

    /// True when neither FIFO holds work and neither engine is mid-command.
    pub fn idle(&self) -> bool {
        self.hq_fifo.is_empty()
            && !self.hq_busy.load(Ordering::Acquire)
            && self.re3_fifo.is_empty()
            && !self.re3_busy.load(Ordering::Acquire)
    }

    /// A FIN2 poll may stall: true until the HQ2 and RE3 have made no
    /// progress for FIN2_STALL_LIMIT (Gr2::fin2_armed_ns).
    pub(super) fn fin2_stall_ok(&self) -> bool {
        let now = self.now_ns().max(1);
        let pos = (self.hq_fifo.consumed() as u64) ^ ((self.re3_fifo.consumed() as u64) << 32);
        if self.fin2_progress.swap(pos, Ordering::AcqRel) != pos {
            self.fin2_armed_ns.store(now, Ordering::Release);
            return true;
        }
        let last = match self.fin2_armed_ns.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => now,
            Err(t) => t,
        };
        now.saturating_sub(last) < FIN2_STALL_LIMIT.as_nanos() as u64
    }

    /// The shell's time, as `set_time` last had it.
    #[inline]
    pub(super) fn now_ns(&self) -> u64 {
        self.now_ns.load(Ordering::Relaxed)
    }

    /// Give the board the shell's time in ns; it never goes back.
    pub fn set_time(&self, ns: u64) {
        self.now_ns.fetch_max(ns, Ordering::Relaxed);
    }

    /// Spin until both engines are idle (tests and snapshots).
    pub fn wait_idle(&self) {
        let backoff = crossbeam_utils::Backoff::new();
        while !self.idle() {
            backoff.snooze();
        }
    }

    /// HQ2 thread: a new HQ2_GEDMA read transfer starts. Words an earlier
    /// one left unread would shift this one, so drop them.
    pub(super) fn gedma_begin(&self) {
        let head = self.gedma_out_head.load(Ordering::Relaxed);
        let tail = self.gedma_out_tail.load(Ordering::Acquire);
        if head != tail {
            crate::dlog_dev!(crate::devlog::LogModule::Gr2,
                "GR2: GEDMA: {} words of the previous transfer never read, dropped", head.wrapping_sub(tail));
            let _ = self.gedma_out_tail.compare_exchange(tail, head, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    /// HQ2 thread: queue one word on the HQ2_GEDMA read port, waiting while
    /// it is full. False if no reader drained it within the limit (or the
    /// engine stops): the rest of the transfer is dropped.
    pub(super) fn gedma_push(&self, w: u32) -> bool {
        let head = self.gedma_out_head.load(Ordering::Relaxed);
        let mut since = 0u64;
        while head.wrapping_sub(self.gedma_out_tail.load(Ordering::Acquire)) as usize >= GEDMA_OUT_WORDS {
            if !self.running.load(Ordering::Relaxed) {
                self.gedma_stall_since.store(0, Ordering::Release);
                return false;
            }
            let now = self.now_ns().max(1);
            if since == 0 {
                since = now;
                self.gedma_stall_since.store(since, Ordering::Release);
            } else if now - since > GEDMA_OUT_STALL_LIMIT_NS {
                crate::dlog_dev!(crate::devlog::LogModule::Gr2, "GR2: GEDMA read port full, no reader: transfer dropped");
                self.gedma_stall_since.store(0, Ordering::Release);
                return false;
            }
            thread::yield_now();
        }
        if since != 0 {
            self.gedma_stall_since.store(0, Ordering::Release);
        }
        self.gedma_out[head as usize & (GEDMA_OUT_WORDS - 1)].store(w, Ordering::Relaxed);
        self.gedma_out_head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Reader: the next HQ2_GEDMA word, if the HQ2 has produced one.
    pub(super) fn gedma_pop(&self) -> Option<u32> {
        let mut tail = self.gedma_out_tail.load(Ordering::Acquire);
        loop {
            if tail == self.gedma_out_head.load(Ordering::Acquire) {
                return None;
            }
            let v = self.gedma_out[tail as usize & (GEDMA_OUT_WORDS - 1)].load(Ordering::Relaxed);
            // CAS: the CPU and the VDMA worker may both read the port.
            match self.gedma_out_tail.compare_exchange(tail, tail.wrapping_add(1), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(v),
                Err(t) => tail = t,
            }
        }
    }

    pub(super) fn unpark(slot: &Mutex<Option<thread::Thread>>) {
        if let Some(t) = slot.lock().as_ref() {
            t.unpark();
        }
    }

    /// After a push: unpark the consumer if it is asleep. The fence pairs
    /// with `sleep`'s: either this sees the thread asleep, or the thread
    /// sees the push before it parks.
    pub(super) fn wake(asleep: &AtomicBool, slot: &Mutex<Option<thread::Thread>>) {
        std::sync::atomic::fence(Ordering::SeqCst);
        if asleep.load(Ordering::Relaxed) {
            Self::unpark(slot);
        }
    }

    /// An engine thread with nothing to do: park for up to 50 ms, unless
    /// `has_work` finds a push made before `asleep` was raised. The timeout
    /// is only a backstop for a wake the fence protocol should never lose,
    /// so it is long enough that an idle engine costs ~20 wakeups/s rather
    /// than 500; a hypothetical missed wake would cost one interval of
    /// latency, never correctness.
    pub(super) fn sleep(asleep: &AtomicBool, has_work: impl FnOnce() -> bool) {
        asleep.store(true, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        if !has_work() {
            thread::park_timeout(std::time::Duration::from_millis(50));
        }
        asleep.store(false, Ordering::Relaxed);
    }

    pub(super) fn wake_hq(&self) {
        Self::wake(&self.hq_asleep, &self.hq_thread);
    }

    pub(super) fn wake_re3(&self) {
        Self::wake(&self.re3_asleep, &self.re3_thread);
    }

    /// Start the HQ2 and RE3 threads. False if they already run.
    pub(super) fn start_engine(&self) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        // SAFETY: Gr2 lives in the machine's Arc for the process lifetime; the
        // threads are joined in stop_engine() before it could be dropped.
        let me: &'static Gr2 = unsafe { std::mem::transmute::<&Gr2, &'static Gr2>(self) };
        let mut threads = self.threads.lock();
        threads.push(thread::Builder::new().name("GR2-HQ2".into()).spawn(move || me.guarded("HQ2", Gr2::hq_loop)).unwrap());
        threads.push(thread::Builder::new().name("GR2-RE3".into()).spawn(move || me.guarded("RE3", Gr2::re3_loop)).unwrap());
        true
    }

    /// Run an engine thread's body; a panic in it is kept and reported to
    /// the shell's hook instead of silently ending the thread.
    pub(super) fn guarded(&self, name: &str, body: fn(&Gr2)) {
        if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(self))) {
            let what = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into());
            let msg = format!("panic on the {name} thread: {what}");
            self.dead.fetch_or(if name == "HQ2" { 1 } else { 2 }, Ordering::AcqRel);
            self.panicked.lock().get_or_insert_with(|| msg.clone());
            if let Some(hook) = &*self.on_panic.lock() {
                hook(&msg);
            }
        }
    }

    /// Tell `hook` (once for each panic) when an engine thread panicked.
    pub fn set_panic_hook(&self, hook: Box<dyn Fn(&str) + Send + Sync>) {
        *self.on_panic.lock() = Some(hook);
    }

    /// What the first engine thread that panicked said, if one did.
    pub fn panic_message(&self) -> Option<String> {
        self.panicked.lock().clone()
    }

    /// Forget a panic, once the threads are stopped and about to start again.
    pub fn clear_panic(&self) {
        *self.panicked.lock() = None;
        self.dead.store(0, Ordering::Release);
    }

    /// Stop the HQ2 and RE3 threads and keep the rest, for a shell that waited for idle: the read
    /// port's words stay for their reader. False if they were not running.
    pub(super) fn park_engine(&self) -> bool {
        if !self.running.swap(false, Ordering::SeqCst) {
            return false;
        }
        // A thread that panicked drains nothing: no sentinel for it, or a full queue would block here.
        let dead = self.dead.load(Ordering::Acquire);
        if dead & 1 == 0 {
            self.hq_fifo.push(u32::MAX, 0);
        }
        if dead & 2 == 0 {
            self.re3_fifo.push(re3::RE3_OP_EXIT, 0);
        }
        Self::unpark(&self.hq_thread);
        Self::unpark(&self.re3_thread);
        for t in self.threads.lock().drain(..) {
            let _ = t.join();
        }
        // Both queues were drained before the sentinels were appended.
        self.hq_fifo.reset();
        self.re3_fifo.reset();
        true
    }

    /// Stop the HQ2 and RE3 threads and discard what is queued behind them.
    pub(super) fn stop_engine(&self) {
        if !self.park_engine() {
            return;
        }
        self.fin3_pending.store(0, Ordering::Release);
    }

    /// A number that moves whenever either engine consumes a queue entry or the read port moves a word:
    /// a shell that sees it still can say the board makes no progress.
    pub fn progress_position(&self) -> u64 {
        let q = (self.hq_fifo.consumed() as u64) ^ ((self.re3_fifo.consumed() as u64) << 32);
        q ^ ((self.gedma_out_tail.load(Ordering::Acquire) as u64) << 16) ^ self.gedma_out_head.load(Ordering::Acquire) as u64
    }

    /// The shell time at which a stall that only time ends gives up: a FIN2 poll held, or the HQ2
    /// holding a transfer for a reader of the read port.
    pub fn stall_deadline(&self) -> Option<u64> {
        let gedma = self.gedma_stall_since.load(Ordering::Acquire);
        let gedma = (gedma != 0).then(|| gedma.saturating_add(GEDMA_OUT_STALL_LIMIT_NS));
        let fin2 = self.fin2_armed_ns.load(Ordering::Acquire);
        let fin2 = (self.fin2_wait.load(Ordering::Acquire) != 0 && fin2 != 0).then(|| fin2.saturating_add(FIN2_STALL_LIMIT.as_nanos() as u64));
        [gedma, fin2].into_iter().flatten().min()
    }

    /// Whether an access at slot offset `off` answers busy now, as `reg_read` and `reg_write` decide
    /// it, without taking a read-port word or moving the FIN2 stall clock.
    pub fn access_would_wait(&self, off: u32, write: bool) -> bool {
        let off = off & (GR2_SLOT_SIZE - 4);
        let hq_working = || !self.hq_fifo.is_empty() || self.hq_busy.load(Ordering::Acquire);
        if write {
            return match off {
                FIFO..FIFO_END | 0x6a068 | 0x6a078 => !self.hq_fifo.has_space(),
                RE3_REGS..RE3_REGS_END | RE3_32 => !self.re3_fifo.has_space(),
                _ => false,
            };
        }
        match off {
            0x6a068 => self.gedma_out_head.load(Ordering::Acquire) == self.gedma_out_tail.load(Ordering::Acquire) && hq_working(),
            o if o == HQREGS + hq2::HQ_VERSION => {
                self.fin2_wait.load(Ordering::Acquire) != 0 && self.regs().hq.fin[hq2::FIN2].load(Ordering::Acquire) == 0 && hq_working()
            }
            RE3_REGS..RE3_REGS_END | RE3_32 => !self.idle(),
            _ => false,
        }
    }

    /// Whether something visible changed since the last `take_dirty`.
    pub fn dirty_pending(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Take the "something visible changed" flag: what the display thread does before each frame.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Note that the picture may differ (a loaded state).
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Words onto the HQ2_GEDMA data port in order: the number the queue had room for.
    pub fn push_gedma(&self, words: &[u32]) -> usize {
        let mut n = 0;
        for &w in words {
            if !self.hq_fifo.try_push(hq2::HQ_TOKEN_GEDMA, w as u64) {
                break;
            }
            n += 1;
        }
        if n != 0 {
            self.wake_hq();
        }
        n
    }

    /// Choose the RE3 span JIT's mode (`re3_jit::MODE_*`) before the threads start.
    #[cfg(feature = "gr2-jit")]
    pub fn set_jit_mode(&self, mode: u32) {
        // SAFETY: the shell calls this with the engine threads not running.
        unsafe { (*self.re3.get()).jit.mode = mode };
    }

    // ── board ID registers ───────────────────────────────────────────────────

    pub(super) fn bdvers_read(&self, idx: u32) -> u32 {
        self.probe.ports.get(idx as usize).copied().unwrap_or(0xff)
    }

    // ── CPU register access (32-bit; narrower widths share the same decode) ──

    pub(super) fn reg_read(&self, off: u32) -> BusRead32 {
        let res = self.reg_read_inner(off);
        if res.status == BUS_OK && !(FIFO..FIFO_END).contains(&off) && self.tracing(debug::TRACE_CPU) {
            self.trace_cpu(false, off, res.data);
        }
        res
    }

    pub(super) fn reg_read_inner(&self, off: u32) -> BusRead32 {
        let r = self.regs();
        let v = match off {
            SHRAM..SHRAM_END => r.shram[(off >> 2) as usize],
            FIFO..FIFO_END => 0,
            HQUCODE..HQUCODE_END => r.hq.ucode[((off - HQUCODE) >> 2) as usize],
            GEWIN..GEWIN_END => r.ge.read(((off - GEWIN) >> 10) as usize, ((off >> 2) & 0xff) as usize),
            // HQ2_GEDMA read: the next word the HQ2 produced (context image,
            // pixel DMA read), read by the kernel's VDMA.
            0x6a068 => {
                // Sample "HQ2 has work" before looking at the port: a word
                // is pushed before the HQ2 goes idle, so an idle HQ2 seen
                // here means every word it produced is visible below.
                let working = !self.hq_fifo.is_empty() || self.hq_busy.load(Ordering::Acquire);
                match self.gedma_pop() {
                    Some(v) => v,
                    // Not produced yet: the request is queued or running.
                    None if working => return BusRead32::busy(),
                    None => {
                        crate::dlog_dev!(crate::devlog::LogModule::Gr2, "GR2: GEDMA read overrun (HQ2 idle, nothing to read)");
                        0
                    }
                }
            }
            0x6b000 => if self.fin3_pending.load(Ordering::Acquire) != 0 { 0 } else { r.hq.fin[hq2::FIN3].load(Ordering::Acquire) },
            HQREGS..HQREGS_END => {
                // Sample "HQ2 has work" before reading the register: the HQ2
                // raises FIN2 before it consumes the entry and drops
                // hq_busy, so if it was idle before the read, the read sees
                // every FIN2 it raised (sampling after would race).
                let hq_working = off - HQREGS == hq2::HQ_VERSION
                    && (!self.hq_fifo.is_empty() || self.hq_busy.load(Ordering::Acquire));
                let v = r.hq.read(off - HQREGS);
                if off - HQREGS == hq2::HQ_VERSION {
                    if self.fin2_wait.load(Ordering::Acquire) != 0 {
                        if v & 2 != 0 {
                            self.fin2_wait.store(0, Ordering::Release);
                        } else if hq_working && self.fin2_stall_ok() {
                            // FIN2 awaited and the HQ2 is still working: the
                            // read waits for it instead of spending the
                            // kernel's poll budget. With the HQ2 idle and no
                            // FIN2, the kernel times out as on hardware.
                            return BusRead32::busy();
                        }
                    }
                    if self.fin3_pending.load(Ordering::Acquire) != 0 {
                        return BusRead32::ok(v & !1);
                    }
                }
                v
            }
            0x6c000..0x6c010 => self.bdvers_read((off - BDVERS) >> 2),
            0x6c040..0x6c060 => r.vc1.read(off & 0x1c),
            0x6c0a0..0x6c100 => r.dac[((off - DAC) >> 5) as usize].read(off & 0x1c) as u32,
            0x6c100..0x6c1a0 => r.xmap[((off - XMAP) >> 5) as usize].read(off & 0x1c),
            0x6c1a0..0x6c1c0 => r.xmap[0].read(off & 0x1c),
            0x6c1c0..0x6c1e0 => r.ab1,
            RE3_REGS..RE3_REGS_END => return self.re3_read(((off - RE3_REGS) >> 2) as usize),
            0x6c600..0x6c604 => return self.re3_read(re3::REG_RWDATA),
            _ => 0, // CLOCK, CC1 (must not echo the flat-panel probe), unused
        };
        BusRead32::ok(v)
    }

    pub(super) fn reg_write(&self, off: u32, val: u32) -> u32 {
        let st = self.reg_write_inner(off, val);
        if st == BUS_OK && !(FIFO..FIFO_END).contains(&off) && self.tracing(debug::TRACE_CPU) {
            self.trace_cpu(true, off, val);
        }
        st
    }

    /// Byte/halfword store. Registers see the value whatever the width; only
    /// the XMAP5 CLUT port distinguishes (a 32-bit store is a packed entry).
    pub(super) fn reg_write_narrow(&self, off: u32, val: u32) -> u32 {
        if (0x6c100..0x6c1c0).contains(&off) && off & 0x1c == xmap5::XMAP_CLUT {
            let r = self.regs();
            if off >= XMAPALL {
                for x in r.xmap.iter_mut() {
                    x.write(xmap5::XMAP_CLUT, val);
                }
            } else {
                r.xmap[((off - XMAP) >> 5) as usize].write(xmap5::XMAP_CLUT, val);
            }
            self.head.damage();
            if self.tracing(debug::TRACE_CPU) {
                self.trace_cpu(true, off, val);
            }
            return BUS_OK;
        }
        self.reg_write(off, val)
    }

    pub(super) fn reg_write_inner(&self, off: u32, val: u32) -> u32 {
        let r = self.regs();
        match off {
            SHRAM..SHRAM_END => r.shram[(off >> 2) as usize] = val,
            FIFO..FIFO_END => {
                let idx = (off - FIFO) >> 2;
                let fin = is_fin3_token(idx);
                if fin {
                    // Counted before the push so the HQ can never see the
                    // token before the counter includes it.
                    self.fin3_pending.fetch_add(1, Ordering::AcqRel);
                }
                if !self.hq_fifo.try_push(idx, val as u64) {
                    if fin {
                        self.fin3_pending.fetch_sub(1, Ordering::AcqRel);
                    }
                    return BUS_BUSY;
                }
                self.wake_hq();
            }
            HQUCODE..HQUCODE_END => r.hq.ucode[((off - HQUCODE) >> 2) as usize] = val,
            GEWIN..GEWIN_END => r.ge.write(((off - GEWIN) >> 10) as usize, ((off >> 2) & 0xff) as usize, val),
            // HQ2_GEDMA: the DMA data port. Words join the command stream, so
            // a command waiting for DMA data (context restore, pixel writes)
            // receives them in order with its FIFO arguments.
            0x6a068 => {
                if !self.hq_fifo.try_push(hq2::HQ_TOKEN_GEDMA, val as u64) {
                    return BUS_BUSY;
                }
                self.wake_hq();
            }
            // FIN3 write port: Xsgi writes 0 after seeing FIN3;
            // the kernel restores a context's saved FIN3 here.
            0x6b000 => r.hq.fin[hq2::FIN3].store(val & 1, Ordering::Release),
            // unstall: restart the microcode. The marker keeps the restart
            // ordered with the FIFO words that follow it (start argument).
            0x6a078 => {
                if !self.hq_fifo.try_push(hq2::HQ_TOKEN_UNSTALL, 0) {
                    return BUS_BUSY;
                }
                r.hq.write(off - HQREGS, val, &mut r.ge);
                self.wake_hq();
            }
            HQREGS..HQREGS_END => {
                if off - HQREGS == hq2::HQ_FIN2 {
                    self.fin2_armed_ns.store(0, Ordering::Release);
                    self.fin2_wait.store(1, Ordering::Release);
                }
                r.hq.write(off - HQREGS, val, &mut r.ge)
            }
            0x6c000..0x6c010 => r.bdvers_w[((off - BDVERS) >> 2) as usize] = val,
            0x6c040..0x6c060 => {
                r.vc1.write(off & 0x1c, val);
                self.head.damage();
            }
            0x6c0a0..0x6c100 => {
                r.dac[((off - DAC) >> 5) as usize].write(off & 0x1c, val as u8);
                self.head.damage();
            }
            0x6c100..0x6c1a0 => {
                let x = &mut r.xmap[((off - XMAP) >> 5) as usize];
                if off & 0x1c == xmap5::XMAP_CLUT { x.write_clut_packed(val) } else { x.write(off & 0x1c, val) }
                self.head.damage();
            }
            0x6c1a0..0x6c1c0 => {
                for x in r.xmap.iter_mut() {
                    if off & 0x1c == xmap5::XMAP_CLUT { x.write_clut_packed(val) } else { x.write(off & 0x1c, val) }
                }
                self.head.damage();
            }
            0x6c1c0..0x6c1e0 => r.ab1 = val,
            // Pixel-clock load, revision 4 and up: record the programmed bytes
            // (the dot clock itself isn't modelled).
            0x6c020..0x6c040 => {
                if r.pll_count < 64 {
                    r.pll_bits |= ((val & 1) as u64) << r.pll_count;
                }
                r.pll_count += 1;
                if val & 2 != 0 {
                    let bytes = r.pll_bits.to_le_bytes();
                    r.pll_last = bytes;
                    r.pll_last[7] = r.pll_count.min(255) as u8;
                    r.pll_programs += 1;
                    r.pll_bits = 0;
                    r.pll_count = 0;
                    if self.tracing(debug::TRACE_CPU) {
                        self.trace_note(&format!("clock PLL programmed: {}", debug::pll_describe(&r.pll_last)));
                    }
                }
            }
            // CC1 flat-panel probe writes 0x00/0x55/0xAA; reads never echo.
            0x6c1e0..0x6c200 => {}
            RE3_REGS..RE3_REGS_END => return self.re3_write(((off - RE3_REGS) >> 2) as usize, val),
            0x6c600..0x6c604 => return self.re3_write(re3::REG_RWDATA, val),
            _ => {
                crate::dlog_dev!(crate::devlog::LogModule::Gr2, "GR2: write {:#07x} = {:#010x} (unmapped)", off, val);
            }
        }
        BUS_OK
    }

    pub(super) fn re3_write(&self, reg: usize, val: u32) -> u32 {
        self.re3_cpu_writes.fetch_add(1, Ordering::Relaxed);
        if !self.re3_fifo.try_push(reg as u32, val as u64) {
            return BUS_BUSY;
        }
        self.wake_re3();
        BUS_OK
    }

    /// RE3 register reads see the pipeline's result, so they wait (bus retry)
    /// until everything queued ahead of them has executed.
    pub(super) fn re3_read(&self, reg: usize) -> BusRead32 {
        if !self.idle() {
            return BusRead32::busy();
        }
        // SAFETY: both FIFOs are drained and both engines idle, so the RE3
        // thread is not touching its state.
        let re3 = unsafe { &*self.re3.get() };
        let v = re3.ctx.reg[reg & 63];
        if reg == re3::REG_RWDATA && re3.ctx.stream == re3::STREAM_READ {
            // Consuming RWDATA advances the READBUF stream to the next pixel.
            if !self.re3_fifo.try_push(re3::RE3_OP_READ_ADVANCE, 0) {
                return BusRead32::busy();
            }
            self.wake_re3();
        }
        BusRead32::ok(v)
    }

    // ── engine threads ───────────────────────────────────────────────────────

    pub(super) fn hq_loop(&self) {
        /// RE3 FIFO entries HQ2 stages before pushing them as one unit.
        const STAGE: usize = 64;
        /// The RE3 FIFO as HQ2 writes it: entries are staged and pushed in
        /// order, as one unit, under one lock and with one wake check: at the
        /// end of each span (the IR write), when the stage is full, before
        /// anything that waits on RE3 or must follow what is queued, and when
        /// HQ2 runs out of work. A span's dozen register writes each took the
        /// FIFO lock and a SeqCst fence (the wake check) before.
        ///
        /// It also skips register writes that would not change the register:
        /// RE3 keeps a register's value until the next write, so resending
        /// it (the span slopes, the colour of a flat polygon, Y along a run
        /// of pieces, ...) only costs FIFO entries. `shadow` holds what HQ2
        /// last sent where `known` has the bit; registers whose write does
        /// more than store (IR, RWDATA, which RE3 also writes, and the RGB /
        /// YX fan-outs) always go, and a CPU write to RE3 forgets it all.
        struct Sink<'a> {
            g: &'a Gr2,
            stage: [(u32, u64); STAGE],
            n: usize,
            shadow: [u32; 64],
            known: u64,
            cpu_writes: u32,
        }
        // Every wait on RE3 gives up once `stop()` clears `running`: RE3 exits
        // on its sentinel, and anything HQ2 queues behind that is never drained.
        impl Sink<'_> {
            fn stopped(&self) -> bool {
                !self.g.running.load(Ordering::Relaxed)
            }
            fn push(&mut self, op: u32, val: u64) {
                if self.n == STAGE {
                    self.flush();
                }
                self.stage[self.n] = (op, val);
                self.n += 1;
            }
            /// Push what is staged and wake RE3.
            fn flush(&mut self) {
                if self.n == 0 {
                    return;
                }
                while !self.g.re3_fifo.try_push_slice(&self.stage[..self.n]) {
                    if self.stopped() {
                        self.n = 0;
                        return;
                    }
                    std::hint::spin_loop();
                }
                self.n = 0;
                self.g.wake_re3();
            }
            /// Wait for RE3 to finish everything queued; false if stopped first.
            fn drain_re3(&mut self) -> bool {
                self.flush();
                while !self.g.re3_fifo.is_empty() || self.g.re3_busy.load(Ordering::Acquire) {
                    if self.stopped() {
                        return false;
                    }
                    std::hint::spin_loop();
                }
                true
            }
        }
        impl Re3Sink for Sink<'_> {
            fn reg(&mut self, reg: usize, val: u32) {
                let r = reg & 63;
                let v = val & re3::REGMASK[r];
                let cpu = self.g.re3_cpu_writes.load(Ordering::Relaxed);
                if cpu != self.cpu_writes {
                    self.cpu_writes = cpu;
                    self.known = 0;
                }
                match r {
                    re3::REG_IR | re3::REG_RWDATA => {}
                    re3::REG_RGB => self.known &= !(1 << re3::REG_R | 1 << re3::REG_G | 1 << re3::REG_B),
                    re3::REG_YX => self.known &= !(1 << re3::REG_X | 1 << re3::REG_Y),
                    _ => {
                        if self.known & (1 << r) != 0 && self.shadow[r] == v {
                            return;
                        }
                        self.shadow[r] = v;
                        self.known |= 1 << r;
                    }
                }
                self.push(reg as u32 | re3::RE3_SRC_HQ, val as u64);
                if r == re3::REG_IR {
                    self.flush();
                }
            }
            fn copy(&mut self, sx: i32, sy: i32, w: i32, h: i32, dx: i32, dy: i32, mode: u32) {
                let a = (sx as u16 as u64) | ((sy as u16 as u64) << 16) | ((w as u16 as u64) << 32) | ((h as u16 as u64) << 48);
                let b = (dx as u16 as u64) | ((dy as u16 as u64) << 16) | ((mode as u64) << 32);
                // One unit: COPY_B executes with the COPY_A before it.
                if self.n + 2 > STAGE {
                    self.flush();
                }
                self.push(re3::RE3_OP_COPY_A, a);
                self.push(re3::RE3_OP_COPY_B, b);
                self.flush();
            }
            fn finish(&mut self, flag: usize) {
                // The host polls the flag to learn the request is done, so it
                // must not be seen before the drawing it follows: drain RE3.
                if !self.drain_re3() {
                    return;
                }
                self.g.regs().hq.fin[flag].store(1, Ordering::Release);
            }
            fn shram(&mut self, word: usize, val: u32) {
                if let Some(w) = self.g.regs().shram.get_mut(word) {
                    *w = val;
                }
            }
            fn gedma_out(&mut self, words: &[u32]) {
                self.flush();
                self.g.gedma_begin();
                for &w in words {
                    if !self.g.gedma_push(w) {
                        break;
                    }
                }
            }
            fn op(&mut self, op: u32, val: u64) {
                self.push(op, val);
            }
            fn read_image(&mut self, req: &hq2::ReadImage, dest: hq2::ReadDest) {
                self.push(re3::RE3_OP_SYNC_BOUNDARY, 0);
                if !self.drain_re3() {
                    return;
                }
                // SAFETY: RE3 is drained and idle; VRAM and Z are only read
                // here, as the display thread does.
                let re3 = unsafe { &mut *self.g.re3.get() };
                re3.record_host_read(req.w as u64 * req.rows as u64);
                self.g.publish_re3_telemetry(re3);
                let plane: &[u32] = if matches!(req.decode, hq2::ReadDecode::Depth { .. }) { &re3.zbuf } else { &re3.vram };
                let read = |x: i32, y: i32| {
                    if x < 0 || y < 0 || x as usize >= re3::FB_W || y as usize >= re3::FB_H {
                        0
                    } else {
                        plane[y as usize * re3::FB_W + x as usize]
                    }
                };
                match dest {
                    hq2::ReadDest::Shram(base) => {
                        req.pack(read, &mut self.g.regs().shram[base..]);
                        self.g.regs().hq.fin[hq2::FIN3].store(1, Ordering::Release);
                    }
                    hq2::ReadDest::Gedma => {
                        let mut words = vec![0u32; req.rows as usize * req.words_per_row as usize];
                        req.pack(read, &mut words);
                        self.gedma_out(&words);
                        // The kernel polls FIN2 once its VDMA has read them.
                        self.g.regs().hq.fin[hq2::FIN2].store(1, Ordering::Release);
                    }
                }
            }
        }
        *self.hq_thread.lock() = Some(thread::current());
        let mut sink = Sink { g: self, stage: [(0, 0); STAGE], n: 0, shadow: [0; 64], known: 0, cpu_writes: 0 };
        let backoff = crossbeam_utils::Backoff::new();
        while self.running.load(Ordering::Relaxed) {
            if let Some((index, val)) = self.hq_fifo.peek() {
                if index == u32::MAX {
                    break;
                }
                self.hq_busy.store(true, Ordering::Release);
                // SAFETY: the HQ2 thread owns the engine.
                let engine = unsafe { &mut *self.hq_engine.get() };
                if self.tracing(debug::TRACE_HQ) {
                    self.trace_hq_entry(index, val as u32);
                    let mut done = |d: String| self.trace_hq_exec(&d);
                    engine.push(index, val as u32, &mut sink, Some(&mut done));
                } else {
                    engine.push(index, val as u32, &mut sink, None);
                }
                if is_fin3_token(index) {
                    // Executed (FIN3 raised if it was a Finish): no longer
                    // pending. Saturating: tests and replays push directly.
                    let _ = self.fin3_pending.try_update(Ordering::AcqRel, Ordering::Acquire,
                        |n| Some(n.saturating_sub(1)));
                }
                self.hq_fifo.consume();
                backoff.reset();
            } else {
                // Out of work: what is staged goes to RE3 before HQ2 reports
                // idle.
                sink.flush();
                self.hq_fifo.flush_head();
                self.hq_busy.store(false, Ordering::Release);
                if backoff.is_completed() {
                    Self::sleep(&self.hq_asleep, || self.hq_fifo.peek().is_some());
                } else {
                    backoff.snooze();
                }
            }
        }
        self.hq_busy.store(false, Ordering::Release);
    }

    pub(super) fn re3_loop(&self) {
        *self.re3_thread.lock() = Some(thread::current());
        let backoff = crossbeam_utils::Backoff::new();
        while self.running.load(Ordering::Relaxed) {
            if let Some((addr, val)) = self.re3_fifo.peek() {
                if addr == re3::RE3_OP_EXIT {
                    break;
                }
                self.re3_busy.store(true, Ordering::Release);
                // SAFETY: the RE3 thread owns `re3` while it runs.
                let re3 = unsafe { &mut *self.re3.get() };
                if self.tracing(debug::TRACE_RE3) {
                    self.trace_re3(addr, val, &re3.ctx.reg);
                }
                let (drew, metrics_changed) = match addr {
                    a if (a & !re3::RE3_SRC_HQ) < 64 => {
                        let drew = re3.write_reg((a & !re3::RE3_SRC_HQ) as usize, val as u32);
                        (drew, drew)
                    }
                    re3::RE3_OP_COPY_A => {
                        self.copy_a.set(val);
                        (false, false)
                    }
                    re3::RE3_OP_COPY_B => {
                        let a = self.copy_a.get();
                        let f = |v: u64, s: u32| (v >> s) as u16 as i16 as i32;
                        let mode = (val >> 32) as u32;
                        if mode == 0 {
                            re3.copy_rect(f(a, 0), f(a, 16), f(a, 32), f(a, 48), f(val, 0), f(val, 16));
                        } else {
                            re3.copy_rect_gl(f(a, 0), f(a, 16), f(a, 32), f(a, 48), f(val, 0), f(val, 16), mode);
                        }
                        (true, true)
                    }
                    re3::RE3_OP_READ_ADVANCE => {
                        re3.read_buffer();
                        (false, true)
                    }
                    re3::RE3_OP_SYNC_BOUNDARY => {
                        re3.record_sync_boundary();
                        (false, true)
                    }
                    re3::RE3_OP_PIXFMT => {
                        re3.set_pixfmt(val as u32);
                        (false, false)
                    }
                    re3::RE3_OP_ZCTL => {
                        re3.set_zctl(val as u32);
                        (false, false)
                    }
                    re3::RE3_OP_STENCIL => {
                        re3.set_stencil(val);
                        (false, false)
                    }
                    re3::RE3_OP_BLEND => {
                        re3.set_blend(val as u32);
                        (false, false)
                    }
                    re3::RE3_OP_ALPHA => {
                        re3.ctx.a = (val as u32) as i64;
                        re3.ctx.da = (val >> 32) as u32 as i32 as i64;
                        (false, false)
                    }
                    re3::RE3_OP_ZFILL_A => {
                        re3.ctx.zfill_rect = val;
                        (false, false)
                    }
                    re3::RE3_OP_ZFILL_B => {
                        let rect = re3.ctx.zfill_rect;
                        re3.zfill(rect, val);
                        (true, true)
                    }
                    _ => (false, false),
                };
                if metrics_changed {
                    self.publish_re3_telemetry(re3);
                }
                if drew {
                    self.head.damage();
                }
                self.re3_fifo.consume();
                backoff.reset();
            } else {
                self.re3_fifo.flush_head();
                self.re3_busy.store(false, Ordering::Release);
                if backoff.is_completed() {
                    Self::sleep(&self.re3_asleep, || self.re3_fifo.peek().is_some());
                } else {
                    backoff.snooze();
                }
            }
        }
        self.re3_busy.store(false, Ordering::Release);
    }

    /// Power-on state of what the probe sets and the DACs.
    pub fn power_on(&self) {
        debug_assert!(!self.running.load(Ordering::Acquire));
        #[cfg(feature = "gr2-jit")]
        let jit_mode = unsafe { (*self.re3.get()).jit.mode };
        // These objects are large and plain data. Clear them in place so a
        // reset cannot overflow a worker thread's stack.
        unsafe {
            std::ptr::write_bytes(self.regs.get(), 0, 1);
            std::ptr::write_bytes(self.re3.get(), 0, 1);
            std::ptr::write_bytes(self.hq_engine.get(), 0, 1);
            (*self.regs.get()).ge.count = self.probe.ges;
            (*self.regs.get()).hq.rev = self.probe.hq_rev;
            #[cfg(feature = "gr2-jit")]
            {
                (*self.re3.get()).jit.mode = jit_mode;
            }
        }
        self.hq_fifo.reset();
        self.re3_fifo.reset();
        self.fin3_pending.store(0, Ordering::Release);
        self.fin2_wait.store(0, Ordering::Release);
        self.fin2_progress.store(0, Ordering::Release);
        self.fin2_armed_ns.store(0, Ordering::Release);
        self.re3_cpu_writes.store(0, Ordering::Release);
        self.published_raster.reset();
        self.presentation_frames.store(0, Ordering::Release);
        self.presentation_host_read_pixels.store(0, Ordering::Release);
        self.gedma_out_head.store(0, Ordering::Release);
        self.gedma_out_tail.store(0, Ordering::Release);
        self.copy_a.set(0);
        self.head.damage();
    }


    pub(super) fn write64_inner(&self, addr: u64, val: u64) -> u32 {
        let off = slot_off(addr) & (GR2_SLOT_SIZE - 8);
        let (hi, lo) = ((val >> 32) as u32, val as u32);
        if (FIFO..FIFO_END).contains(&off) {
            // Both words or neither: a retried store must not push the first twice.
            let idx = (off - FIFO) >> 2;
            let fins = is_fin3_token(idx) as u32 + is_fin3_token(idx + 1) as u32;
            if fins != 0 {
                self.fin3_pending.fetch_add(fins, Ordering::AcqRel);
            }
            if !self.hq_fifo.try_push2(idx, hi as u64, idx + 1, lo as u64) {
                if fins != 0 {
                    self.fin3_pending.fetch_sub(fins, Ordering::AcqRel);
                }
                return BUS_BUSY;
            }
            self.wake_hq();
            return BUS_OK;
        }
        if (RE3_REGS..RE3_REGS_END).contains(&off) {
            let reg = ((off - RE3_REGS) >> 2) as u32;
            self.re3_cpu_writes.fetch_add(1, Ordering::Relaxed);
            if !self.re3_fifo.try_push2(reg, hi as u64, reg + 1, lo as u64) {
                return BUS_BUSY;
            }
            self.wake_re3();
            return BUS_OK;
        }
        let st = self.reg_write(off, hi);
        if st != BUS_OK {
            return st;
        }
        self.reg_write(off + 4, lo)
    }
}

/// Longest a version read stalls for an awaited FIN2 (see Gr2::fin2_wait).
pub(super) const FIN2_STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// FIFO tokens that raise FIN3 when executed (GL Finish, 2D sync).
pub(super) fn is_fin3_token(idx: u32) -> bool {
    idx == hq2::GL_FINISH || idx == hq2::HQ_GL_FIN3
}
