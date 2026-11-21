//! The IMPACT engine from the HQ3 command FIFO down: the command parser, the
//! GE11 stand-in, the raster engines, the pixel processor and the texture
//! engine, behind `BoardEngine`.
//!
//! Threads, locks and calls:
//! - The shell calls from one thread at a time (QEMU: the thread holding the
//!   BQL). The engine takes no shell lock and never calls back into the
//!   shell, but for the notify hook, which only schedules. The rest of the
//!   call contract is in the `sgi_engine` crate docs.
//! - `write` and `push_words` stage operations on the caller's side; `advance`
//!   (or a full stage) hands them to the queue. Status reads are answered from
//!   counters and never wait. Any other read answers `Busy` while work is
//!   queued.
//! - `Mode::Threaded` runs queued operations on one worker thread. It reads
//!   and writes host memory through `HostMem::ram_*` only, which must be
//!   lock-free. `Mode::Inline` has no thread: `run` executes the queue, in
//!   order, on the caller's thread, so replays and parity tests do not depend
//!   on host scheduling, and its host DMA goes through `HostMem::bus_*`.
//! - Host DMA addresses are fabric addresses: the XIO widget of the transfer's
//!   page table (bits 19:16 of its control word) in bits 51:48, above the node's
//!   40-bit physical address.
//! - The worker holds `Shared::inner` while it executes; the caller takes it
//!   only when the queue is empty and the worker parked between batches.
//!   Lock order: `inner`, then `outbox`; `q` is never held with either.
//! - Events go to an outbox the shell drains. A panic in the worker or in
//!   `run` becomes a device fault: logged once, the engine stops, and every
//!   call answers `Fault` until `reset`.
//! - MGRAS never stalls on the shell: its stream does not wait for a frame or
//!   a time, so `Idle` is `Yes` or `Working` (and `Faulted`).
//! - One thread steps both raster engines of a Maximum Impact, so drawing
//!   does not use two cores as iris's board does.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};

use super::back::ImpactBack;
use super::display::{self, Display};
use super::hostmem::{Fault as DmaFault, HostMem as DmaMem};
use super::hq3::{host, Hq3Board, Hq3Engine, Hq3Regs, Hq3Sink, Hq3Stats};
use super::pixmem::{Buffer, Kind, PAGE_WORDS};
use super::plain::boxed_zeroed;
use super::quirks::Hq4Quirks;
use super::rss::{self, Entry, Rss};
use super::te1::Te1;
use crate::display::{Mark, Rect};
use sgi_engine::inject::{hit, Point};
use sgi_engine::{BoardEngine, Chunk, Event, Fault, FrameWords, HeadTiming, HostMem, Idle, LentFrame, Outbox, Pump, Read, Refused, StateSink, StateSource, Swap, Write, DAMAGE_ANY};
use crate::wgpu::Gpu;

/// The boards that share this engine; they differ in the geometry engine
/// and raster engine count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Board {
    Solid,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// No thread: `run` executes the queue.
    Inline,
    /// One worker thread executes the queue.
    Threaded,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub board: Board,
    /// The Octane's HQ4 front: its microcode's GE layout and read order.
    pub octane: bool,
    /// Bytes of a DMA page-table entry: 4, or 8 on the Origin 2000 and Onyx2.
    pub entry_bytes: u32,
    /// The two board-version bytes the display bus answers; None for the board's own.
    pub bdvers: Option<[u32; 2]>,
    /// The two colour maps' revision registers.
    pub cmap_rev: [u32; 2],
    /// The pixel processor revision the XMAP reports under index 4, bits 2:0; None takes the board's from
    /// `Config::pp1_revision`.
    pub pp1_revision: Option<u32>,
    /// What the GE readback registers hold until the first GE query is answered: the GE revision
    /// the kernel reads at 0x028004 (2 is a GE11 rev B, 3 a GE12 rev A). 0 reads as no GE.
    pub ge_revision: u32,
    pub mode: Mode,
    /// Operations queued before `write` answers `Busy`.
    pub queue_ops: usize,
    /// `rss_jit::MODE_*`; None leaves the crate's default.
    #[cfg(feature = "gr4-jit")]
    pub jit: Option<u32>,
}

impl Config {
    /// The pixel processor revision the XMAP reports: the one given, else the board's by the captured boards
    /// (gr4 c86de26 devices/mgras/board-identity.md, inferred from the owners' `gfxinfo` output): revision A (0) on
    /// the GE11 boards, E (4) on a one-GE GE12 board (the ESI) and H (7) on a two-GE one (the EMXI, the ESSI).
    /// The GE revision is the one the kernel reads at 0x028004 (2 a GE11, 3 and up a GE12); none given is A.
    pub fn pp1_revision(&self) -> u32 {
        self.pp1_revision.unwrap_or(match (self.ge_revision, self.board) {
            (0..=2, _) => 0,
            (_, Board::Max) => 7,
            _ => 4,
        })
    }

    pub fn new(board: Board, mode: Mode) -> Config {
        Config {
            board,
            octane: false,
            entry_bytes: 4,
            bdvers: None,
            cmap_rev: [0xFB, 0xFB],
            pp1_revision: None,
            ge_revision: 0,
            mode,
            queue_ops: 1 << 16,
            #[cfg(feature = "gr4-jit")]
            jit: None,
        }
    }
}

/// A box of pixels in one buffer.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub buffer: Buffer,
    pub rect: Rect,
}

#[derive(Clone, Copy)]
enum Op {
    Word { port: u8, w: u32 },
    Raster { reg: u16, val: u32, exec: bool },
    Ucode { index: u16, w: u32 },
    /// A register the engine only keeps (`Hq3Regs::regs`).
    Reg { off: u32, val: u32 },
    ContextSwitch,
}

/// Ops a stage holds before it is published to the queue.
const STAGE_MAX: usize = 256;
/// Ops the worker takes from the queue at a time.
const BATCH_MAX: usize = 4096;
/// Distinct damage marks waiting before they degrade to "anything".
const DAMAGE_MAX: usize = 16;

/// The registers the shell reads without waiting: the HQ flags and the
/// enables that gate them. The worker sets flags too.
#[derive(Default)]
struct Front {
    flags: AtomicU32,
    flag_enable: AtomicU32,
    interrupt_enable: AtomicU32,
}

impl Front {
    /// The general interrupt line (GIO line 1): up while an enabled flag is
    /// set. Flags the front derives from the GE11's output are not here.
    fn irq_levels(&self) -> u64 {
        (self.flags.load(Ordering::Acquire) & self.interrupt_enable.load(Ordering::Acquire) & host::INTR_CAUSES != 0) as u64
    }
}

struct Queue {
    ops: VecDeque<Op>,
    stop: bool,
}

struct Shared<M: HostMem> {
    inner: Mutex<Inner<M>>,
    q: Mutex<Queue>,
    /// The worker waits on `work_cv` for work, the caller on `idle_cv`.
    work_cv: Condvar,
    idle_cv: Condvar,
    /// Operations, and FIFO words among them, finished.
    done: AtomicU64,
    words_done: AtomicU64,
    /// Wall time the worker thread has spent executing batches, in ns.
    busy_ns: AtomicU64,
    /// Moves whenever the picture may have changed (`frame_generation`).
    generation: AtomicU64,
    front: Arc<Front>,
    out: Arc<Outbox>,
    fault: Mutex<Option<String>>,
    dead: AtomicBool,
}

impl<M: HostMem> Shared<M> {
    fn fail(&self, what: &str) {
        let mut f = self.fault.lock();
        if f.is_none() {
            log::error!("mgras engine: {what}");
            *f = Some(what.to_string());
            drop(f);
            self.dead.store(true, Ordering::Release);
            self.out.post(Event::DeviceFault(what.to_string()));
        }
    }

    /// Execute `ops` and count them done. A panic stops the engine; ops
    /// after it are counted without running.
    fn execute(&self, ops: &mut Vec<Op>) {
        let n = ops.len() as u64;
        let words = ops.iter().filter(|o| matches!(o, Op::Word { .. })).count() as u64;
        if !self.dead.load(Ordering::Acquire) {
            let r = catch_unwind(AssertUnwindSafe(|| {
                let mut inner = self.inner.lock();
                for op in ops.iter() {
                    hit(Point::Exec);
                    inner.exec(*op);
                }
                if inner.finish_batch() {
                    self.generation.fetch_add(1, Ordering::Release);
                }
            }));
            if let Err(p) = r {
                self.fail(&format!("the engine panicked: {}", panic_text(&*p)));
            }
        }
        ops.clear();
        let _q = self.q.lock();
        self.words_done.fetch_add(words, Ordering::Release);
        self.done.fetch_add(n, Ordering::Release);
        self.idle_cv.notify_all();
    }
}

fn mark(m: Mark) -> sgi_engine::Mark {
    sgi_engine::Mark { store: m.store, planes: m.planes, rect: sgi_engine::Rect::new(m.rect.x0, m.rect.y0, m.rect.x1, m.rect.y1) }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

fn worker<M: HostMem>(sh: Arc<Shared<M>>) {
    let mut batch = Vec::with_capacity(BATCH_MAX);
    loop {
        {
            let mut q = sh.q.lock();
            while q.ops.is_empty() && !q.stop {
                sh.work_cv.wait(&mut q);
            }
            if q.ops.is_empty() {
                return;
            }
            let n = q.ops.len().min(BATCH_MAX);
            batch.extend(q.ops.drain(..n));
        }
        let began = std::time::Instant::now();
        sh.execute(&mut batch);
        sh.busy_ns.fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        // A shell access that found the engine busy waits for this notify.
        sh.out.wake();
    }
}

/// The parser and the raster side, with what they reach: host memory and
/// the outbox.
struct Inner<M: HostMem> {
    hq: Hq3Engine,
    core: Core<M>,
}

// SAFETY: the raster engines hold raw pointers into planes this value owns,
// and the GPU unit is single-owner; the value is used by one thread at a
// time, under `Shared::inner`.
unsafe impl<M: HostMem> Send for Inner<M> {}

struct Core<M: HostMem> {
    back: ImpactBack,
    regs: Box<Hq3Regs>,
    mem: Arc<M>,
    out: Arc<Outbox>,
    front: Arc<Front>,
    /// Damage since the batch began, posted by `finish_batch`.
    damage: Vec<Mark>,
    /// Nothing runs on a worker: DMA may go through the bus.
    inline: bool,
    /// The XIO widget of the transfer in progress (`DmaPath`).
    dma_widget: u32,
}

impl<M: HostMem> Inner<M> {
    fn new(cfg: &Config, gpu: Option<&Arc<Gpu>>, mem: Arc<M>, out: Arc<Outbox>, front: Arc<Front>) -> Result<Inner<M>, String> {
        let mut hq = Hq3Engine::new(if cfg.board == Board::Max { 2 } else { 1 });
        hq.entry_bytes = cfg.entry_bytes;
        if cfg.octane {
            hq.kick_starts_read = true;
            hq.board = Some(Box::new(Hq4Quirks) as Box<dyn Hq3Board>);
        }
        let mut back = ImpactBack::with_engines(gpu, if cfg.board == Board::Max && gpu.is_none() { 2 } else { 1 })?;
        #[cfg(feature = "gr4-jit")]
        if let Some(m) = cfg.jit {
            for r in &mut back.rss {
                r.jit.mode = m;
            }
        }
        let mut regs = boxed_zeroed::<Hq3Regs>();
        regs.ge_readback = [0, cfg.ge_revision];
        Ok(Inner { hq, core: Core { back, regs, mem, out, front, damage: Vec::new(), inline: cfg.mode == Mode::Inline, dma_widget: 0 } })
    }

    fn exec(&mut self, op: Op) {
        match op {
            Op::Word { port, w } => self.hq.push(port as usize, w, &mut self.core),
            Op::Raster { reg, val, exec } => self.core.rss_write(reg as u32, val, exec),
            Op::Ucode { index, w } => {
                self.core.regs.ucode[index as usize] = w;
                self.core.regs.ucode_hash.word(index as usize, w);
            }
            Op::Reg { off, val } => {
                self.core.regs.regs.insert(off, val);
            }
            Op::ContextSwitch => self.hq.begin_context_switch(&mut self.core),
        }
    }

    /// The end of a run of operations: submit GPU work and post damage.
    /// True when pixels were drawn.
    fn finish_batch(&mut self) -> bool {
        self.core.back.rss[0].gpu_flush();
        let marks: Vec<sgi_engine::Mark> = std::mem::take(&mut self.core.damage).into_iter().map(mark).collect();
        self.core.out.damage(&marks);
        !marks.is_empty()
    }

    fn read(&mut self, off: u32, bits: u32) -> u64 {
        match off {
            host::RASTER_IF_CONTEXT..=0x50228 => self.hq.raster_if_regs[((off - host::RASTER_IF_CONTEXT) / 4) as usize] as u64,
            host::DMA_CONTEXT..=0x504FF => self.hq.dma_regs[((off - host::DMA_CONTEXT) / 4) as usize] as u64,
            host::PIO_READ_HI => {
                let rss = &mut self.core.back.rss[0];
                let _ = rss.gpu_sync_transfer(super::wgpu::Observation::PioRead);
                let v = rss.pio_peek_hi();
                self.core.back.run(Entry::PioAdvance);
                v as u64
            }
            host::PIO_READ_LO => self.core.back.rss[0].pio_read_lo() as u64,
            host::GE_READBACK_HI | host::GE_READBACK_LO => match self.hq.ge_return {
                Some(r) => r[(off == host::GE_READBACK_LO) as usize] as u64,
                None => self.core.regs.ge_readback[(off == host::GE_READBACK_LO) as usize] as u64,
            },
            host::RASTER..=0x7FFFF => {
                let rss = &mut self.core.back.rss[0];
                let r = (off & 0xFFC) >> 2;
                if bits == 64 {
                    ((rss.read(r) as u64) << 32) | rss.read(r + 1) as u64
                } else {
                    rss.read(r) as u64
                }
            }
            host::UCODE..=0x45FFF => self.core.regs.ucode[((off - host::UCODE) / 4) as usize] as u64,
            _ => self.core.regs.regs.get(off) as u64,
        }
    }
}

impl<M: HostMem> Core<M> {
    fn rss_write(&mut self, r: u32, val: u32, exec: bool) {
        if self.back.run(Entry::Write { reg: r, val, exec }) {
            self.drawn();
        }
    }

    fn drawn(&mut self) {
        let damage = &mut self.damage;
        for rss in &self.back.rss {
            rss.drawn(|b, planes, rect| {
                let store = b.ptr as u64 | ((b.kind == Kind::Overlay) as u64) << 16;
                if let Some(m) = damage.iter_mut().find(|m| m.store == store && m.planes == planes) {
                    m.rect = m.rect.union(&rect);
                } else if damage.len() < DAMAGE_MAX {
                    damage.push(Mark { store, planes, rect });
                } else {
                    let any = damage.iter().fold(rect, |a, m| a.union(&m.rect));
                    damage.clear();
                    damage.push(Mark { store: DAMAGE_ANY, planes: !0, rect: any });
                }
            });
        }
    }
}

/// The shift of the XIO widget in a fabric address: the widget in bits 51:48 above the node's 40-bit physical address.
const WIDGET_SHIFT: u32 = 48;

/// Host memory as the command parser's DMA sees it, at fabric addresses (`widget << 48 | node address`). On the
/// worker thread it takes the RAM path only; inline, where the transfer runs on the caller's thread inside a call,
/// it goes through the bus.
struct DmaPath<M: HostMem> {
    mem: Arc<M>,
    widget: u64,
    bus: bool,
}

impl<M: HostMem> DmaPath<M> {
    fn at(&self, addr: u64) -> u64 {
        addr | self.widget << WIDGET_SHIFT
    }
}

impl<M: HostMem> DmaMem for DmaPath<M> {
    fn read32(&self, addr: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b).ok()?;
        Some(u32::from_be_bytes(b))
    }
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), DmaFault> {
        let at = self.at(addr);
        let done = if self.bus { self.mem.bus_read(at, out, Swap::None) } else { self.mem.ram_read(at, out, Swap::None) };
        done.map_err(|_| {
            out.fill(0);
            DmaFault
        })
    }
    fn write(&self, addr: u64, data: &[u8]) -> Result<(), DmaFault> {
        let at = self.at(addr);
        let done = if self.bus { self.mem.bus_write(at, data, Swap::None) } else { self.mem.ram_write(at, data, Swap::None) };
        done.map_err(|_| DmaFault)
    }
}

impl<M: HostMem> Hq3Sink for Core<M> {
    fn rss_write(&mut self, r: u32, val: u32, exec: bool) {
        Core::rss_write(self, r, val, exec);
    }
    fn transfer(&mut self) -> Option<(bool, (u32, u32))> {
        let rss = &*self.back.rss[0];
        Some((rss.transfer_armed()?, rss.transfer_shape()?))
    }
    fn dma_write_line(&mut self, line: u32, bytes: &[u8]) {
        self.back.run(Entry::DmaLine { line, bytes });
        self.drawn();
    }
    fn dma_read_line(&mut self, line: u32) -> Vec<u8> {
        let _ = self.back.rss[0].gpu_sync_transfer(super::wgpu::Observation::DmaRead);
        self.back.rss[0].dma_read_line(line)
    }
    fn gl_bracket(&mut self, enter: bool) {
        self.back.run(if enter { Entry::GlEnter } else { Entry::GlLeave });
    }
    fn finish_raster(&mut self) {
        self.back.rss[0].gpu_flush();
    }
    fn set_flags(&mut self, bits: u32) {
        self.front.flags.fetch_or(bits, Ordering::AcqRel);
        self.out.post(Event::Irq);
    }
    fn take_flags(&mut self, mask: u32) -> u32 {
        let was = self.front.flags.fetch_and(!mask, Ordering::AcqRel) & mask;
        self.out.post(Event::Irq);
        was
    }
    fn sys_mem(&mut self) -> Option<Arc<dyn DmaMem>> {
        Some(Arc::new(DmaPath { mem: self.mem.clone(), widget: self.dma_widget as u64, bus: self.inline }))
    }
    fn set_dma_widget(&mut self, widget: u32) {
        self.dma_widget = widget;
    }
    fn host_fault(&mut self, addr: u64, write: bool) {
        self.out.post(Event::BusFault { addr, write });
    }
    fn note(&mut self, what: String) {
        self.out.post(Event::Note(what));
    }
}

/// The small state of a saved engine; the rest is chunks of its own.
#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    board: Board,
    engines: u32,
    flags: u32,
    flag_enable: u32,
    interrupt_enable: u32,
}

const STATE_VERSION: u32 = 2;
const CID_CHUNK: usize = 4096;

fn chunk(name: &str, index: u64, bytes: Vec<u8>) -> Chunk {
    Chunk { name: name.into(), index, bytes }
}

impl<M: HostMem> Inner<M> {
    /// The state as chunks: `meta`, `front` (`Hq3Regs` as bytes), `hq3`, one
    /// `rss` per raster engine, `te1`, and the pixel memory by page and the
    /// clip IDs by 4 KiB, all-zero pieces left out.
    fn save(&mut self, board: Board, display: &Display, sink: &mut dyn StateSink) -> Result<(), Refused> {
        let bad = |e: String| Refused::Faulted(e);
        for r in self.core.back.rss.iter_mut() {
            r.gpu_flush();
            r.gpu_sync(
                #[cfg(feature = "wgpu")]
                super::wgpu::Observation::Snapshot,
            )
            .map_err(bad)?;
        }
        let f = &self.core.front;
        let meta = Meta {
            version: STATE_VERSION,
            board,
            engines: self.core.back.rss.len() as u32,
            flags: f.flags.load(Ordering::Acquire),
            flag_enable: f.flag_enable.load(Ordering::Acquire),
            interrupt_enable: f.interrupt_enable.load(Ordering::Acquire),
        };
        let put = |sink: &mut dyn StateSink, c: Chunk| sink.put(c).map_err(|e| Refused::Faulted(e.0));
        put(sink, chunk("meta", 0, postcard::to_allocvec(&meta).map_err(|e| Refused::Faulted(e.to_string()))?))?;
        // SAFETY: `Hq3Regs` is plain numeric state allocated zeroed, so its
        // padding is initialised.
        let front = unsafe { std::slice::from_raw_parts((&*self.core.regs as *const Hq3Regs).cast::<u8>(), std::mem::size_of::<Hq3Regs>()).to_vec() };
        put(sink, chunk("front", 0, front))?;
        put(sink, chunk("hq3", 0, self.hq.snapshot_bytes().map_err(bad)?))?;
        for (i, r) in self.core.back.rss.iter().enumerate() {
            put(sink, chunk("rss", i as u64, r.snapshot_bytes().map_err(bad)?))?;
        }
        let planes = self.core.back.planes();
        put(sink, chunk("te1", 0, planes.te.snapshot_bytes().map_err(bad)?))?;
        for (i, w) in planes.mem.words.chunks_exact(PAGE_WORDS).enumerate().filter(|(_, w)| w.iter().any(|&x| x != 0)) {
            put(sink, chunk("pixels", i as u64, w.iter().flat_map(|x| x.to_le_bytes()).collect()))?;
        }
        for (i, c) in planes.cid.chunks(CID_CHUNK).enumerate().filter(|(_, c)| c.iter().any(|&x| x != 0)) {
            put(sink, chunk("cid", i as u64, c.to_vec()))?;
        }
        put(sink, chunk("dcb", 0, display.snapshot_bytes()))?;
        Ok(())
    }

    /// Check the whole state, then apply it.
    fn load(&mut self, board: Board, display: &mut Display, src: &mut dyn StateSource) -> Result<(), Refused> {
        let bad = |m: String| Refused::Mismatch(m);
        let (mut meta, mut front, mut hq3, mut te1, mut dcb) = (None, None, None, None, None);
        let (mut rss, mut pixels, mut cid) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        while let Some(c) = src.next().map_err(|e| Refused::Mismatch(e.0))? {
            let dup = |first: bool| if first { Ok(()) } else { Err(bad(format!("state has two {} chunks", c.name))) };
            match c.name.as_str() {
                "meta" => dup(meta.replace(postcard::from_bytes::<Meta>(&c.bytes).map_err(|e| bad(e.to_string()))?).is_none())?,
                "front" => dup(front.replace(c.bytes).is_none())?,
                "hq3" => dup(hq3.replace(c.bytes).is_none())?,
                "te1" => dup(te1.replace(c.bytes).is_none())?,
                "dcb" => dup(dcb.replace(c.bytes).is_none())?,
                "rss" => dup(rss.insert(c.index, c.bytes).is_none())?,
                "pixels" => dup(pixels.insert(c.index, c.bytes).is_none())?,
                "cid" => dup(cid.insert(c.index, c.bytes).is_none())?,
                other => return Err(bad(format!("unknown state chunk {other}"))),
            }
        }
        let (Some(meta), Some(front), Some(hq3), Some(te1), Some(dcb)) = (meta, front, hq3, te1, dcb) else {
            return Err(bad("state lacks a part".into()));
        };
        let engines = self.core.back.rss.len();
        if meta.version != STATE_VERSION || meta.board != board || meta.engines as usize != engines || rss.len() != engines {
            return Err(bad(format!("state is version {} for {:?} with {} raster engines, this is version {STATE_VERSION} for {board:?} with {engines}", meta.version, meta.board, meta.engines)));
        }
        if front.len() != std::mem::size_of::<Hq3Regs>() {
            return Err(bad(format!("front registers are {} bytes, expected {}", front.len(), std::mem::size_of::<Hq3Regs>())));
        }
        let hq = Hq3Engine::decode_snapshot(&hq3, if board == Board::Max { 2 } else { 1 }).map_err(bad)?;
        let rss = (0..engines as u64)
            .map(|i| Rss::decode_snapshot(rss.get(&i).ok_or_else(|| bad(format!("state lacks raster engine {i}")))?).map_err(bad))
            .collect::<Result<Vec<_>, _>>()?;
        let te = Te1::decode_snapshot(&te1).map_err(bad)?;
        let pages = self.core.back.planes().mem.words.len() / PAGE_WORDS;
        for (i, w) in &pixels {
            if *i as usize >= pages || w.len() != PAGE_WORDS * 8 {
                return Err(bad(format!("pixel page {i} has {} bytes", w.len())));
            }
        }
        let cid_len = self.core.back.planes().cid.len();
        for (i, c) in &cid {
            if *i as usize * CID_CHUNK + c.len() > cid_len {
                return Err(bad(format!("clip-ID chunk {i} runs past the plane")));
            }
        }
        if dcb.len() != std::mem::size_of::<super::dcb::Dcb>() {
            return Err(bad(format!("display bus state is {} bytes, expected {}", dcb.len(), std::mem::size_of::<super::dcb::Dcb>())));
        }
        // SAFETY: the length was checked and `Hq3Regs` is plain numeric state.
        unsafe { std::ptr::copy_nonoverlapping(front.as_ptr(), (&mut *self.core.regs as *mut Hq3Regs).cast::<u8>(), front.len()) };
        self.hq.apply_snapshot(hq);
        for (r, st) in self.core.back.rss.iter_mut().zip(rss) {
            r.apply_snapshot(st);
        }
        let planes = self.core.back.planes_mut();
        planes.te.apply_snapshot(te);
        planes.mem.words.fill(0);
        for (i, bytes) in &pixels {
            let dst = &mut planes.mem.words[*i as usize * PAGE_WORDS..][..PAGE_WORDS];
            for (d, b) in dst.iter_mut().zip(bytes.chunks_exact(8)) {
                *d = u64::from_le_bytes(b.try_into().unwrap());
            }
        }
        planes.cid.fill(0);
        for (i, c) in &cid {
            planes.cid[*i as usize * CID_CHUNK..][..c.len()].copy_from_slice(c);
        }
        self.core.back.reattach_gpu();
        display.apply_snapshot(&dcb).map_err(bad)?;
        let f = &self.core.front;
        f.flags.store(meta.flags, Ordering::Release);
        f.flag_enable.store(meta.flag_enable, Ordering::Release);
        f.interrupt_enable.store(meta.interrupt_enable, Ordering::Release);
        self.core.damage.clear();
        self.core.out.damage_all();
        self.core.out.post(Event::Irq);
        Ok(())
    }
}

/// The MGRAS engine. `M` is the shell's host memory.
pub struct MgrasEngine<M: HostMem> {
    sh: Arc<Shared<M>>,
    cfg: Config,
    gpu: Option<Arc<Gpu>>,
    mem: Arc<M>,
    /// The display bus and the CPU frame; lock order: `Shared::inner`, then this.
    display: Mutex<Display>,
    stage: Vec<Op>,
    /// Operations accepted so far, staged and queued, and FIFO words among
    /// them; the worker's counters are `done` and `words_done`.
    submitted: u64,
    words: u64,
    worker: Option<JoinHandle<()>>,
    parked: bool,
    now: u64,
}

impl<M: HostMem> MgrasEngine<M> {
    /// `notify` is called when work waits (staged operations) or an event
    /// is posted, from any thread, and must only schedule the shell's pump.
    pub fn new(cfg: Config, mem: M, gpu: Option<&Arc<Gpu>>, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Result<Self, String> {
        let mem = Arc::new(mem);
        let out = Arc::new(Outbox::new(notify));
        let front = Arc::new(Front::default());
        let inner = Inner::new(&cfg, gpu, mem.clone(), out.clone(), front.clone())?;
        let sh = Arc::new(Shared {
            inner: Mutex::new(inner),
            q: Mutex::new(Queue { ops: VecDeque::new(), stop: false }),
            work_cv: Condvar::new(),
            idle_cv: Condvar::new(),
            done: AtomicU64::new(0),
            words_done: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            front,
            out,
            fault: Mutex::new(None),
            dead: AtomicBool::new(false),
        });
        let display = Mutex::new(Display::new(cfg.bdvers.unwrap_or_else(|| display::default_bdvers(cfg.board)), cfg.cmap_rev, cfg.pp1_revision()));
        let mut e = MgrasEngine { sh, cfg, gpu: gpu.cloned(), mem, display, stage: Vec::new(), submitted: 0, words: 0, worker: None, parked: false, now: 0 };
        e.spawn();
        Ok(e)
    }

    pub fn mem(&self) -> &Arc<M> {
        &self.mem
    }

    /// The time the shell last gave, ns.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// What a GPU unit's device-loss callback calls once this engine exists (`GpuLoss::attach`):
    /// post `GpuLost`, then fault the engine, which tells the shell.
    pub fn gpu_lost_handler(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let sh = self.sh.clone();
        move |why| {
            sh.out.post(Event::GpuLost(why.to_string()));
            sh.fail(why);
        }
    }

    /// The HQ flags as the board holds them.
    pub fn flags(&self) -> u32 {
        self.sh.front.flags.load(Ordering::Acquire)
    }

    fn spawn(&mut self) {
        if self.cfg.mode == Mode::Threaded && self.worker.is_none() {
            self.sh.q.lock().stop = false;
            let sh = self.sh.clone();
            self.worker = Some(std::thread::Builder::new().name("MGRAS-engine".into()).spawn(move || worker(sh)).expect("spawn the engine worker"));
        }
    }

    fn stop_worker(&mut self) {
        if let Some(w) = self.worker.take() {
            self.sh.q.lock().stop = true;
            self.sh.work_cv.notify_all();
            let _ = w.join();
        }
    }

    fn dead(&self) -> bool {
        self.sh.dead.load(Ordering::Acquire)
    }

    fn pending_ops(&self) -> u64 {
        self.submitted - self.sh.done.load(Ordering::Acquire)
    }

    fn is_idle(&self) -> bool {
        self.pending_ops() == 0
    }

    /// Hand the stage to the queue.
    fn flush(&mut self) {
        if self.stage.is_empty() {
            return;
        }
        self.sh.q.lock().ops.extend(self.stage.drain(..));
        if self.worker.is_some() {
            self.sh.work_cv.notify_one();
        }
    }

    /// Whether `n` more operations fit; publishes the stage to find out.
    fn room(&mut self, n: u64) -> bool {
        let limit = self.cfg.queue_ops as u64;
        if self.pending_ops() + n <= limit {
            return true;
        }
        self.flush();
        self.pending_ops() + n <= limit
    }

    fn stage_op(&mut self, op: Op) {
        if self.stage.is_empty() {
            self.sh.out.wake();
        }
        if let Op::Word { .. } = op {
            self.words += 1;
        }
        self.stage.push(op);
        self.submitted += 1;
        if self.stage.len() >= STAGE_MAX {
            self.flush();
        }
    }

    /// Run `max` operations of the queue on this thread.
    fn run_ops(&mut self, max: usize) -> usize {
        self.flush();
        let mut ran = 0;
        let mut batch = Vec::new();
        while ran < max {
            {
                let mut q = self.sh.q.lock();
                let n = q.ops.len().min(BATCH_MAX).min(max - ran);
                if n == 0 {
                    break;
                }
                batch.extend(q.ops.drain(..n));
            }
            ran += batch.len();
            self.sh.execute(&mut batch);
        }
        ran
    }

    /// Wait for the queue to empty, for `budget` of host time at most. With
    /// no worker running (inline mode, or parked) it runs the queue here.
    /// False on timeout or fault.
    fn settle(&mut self, budget: Option<Duration>) -> bool {
        self.flush();
        let deadline = budget.map(|b| Instant::now() + b);
        if self.worker.is_none() {
            while self.run_ops(BATCH_MAX) == BATCH_MAX && deadline.is_none_or(|d| Instant::now() < d) {}
            return self.is_idle() && !self.dead();
        }
        let mut q = self.sh.q.lock();
        while self.submitted > self.sh.done.load(Ordering::Acquire) {
            match deadline {
                Some(d) => {
                    if self.sh.idle_cv.wait_until(&mut q, d).timed_out() {
                        break;
                    }
                }
                None => self.sh.idle_cv.wait(&mut q),
            }
        }
        self.is_idle() && !self.dead()
    }

    fn status(&self, off: u32) -> u32 {
        match off {
            host::STATUS => (if self.is_idle() { host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE } else { 0 }) | host::STATUS_FIXED | host::STATUS_CFIFO_LOW,
            host::FIFOSTATUS => ((self.words - self.sh.words_done.load(Ordering::Acquire)) as u32).min(0x7E),
            _ => 0,
        }
    }

    /// The raster register `r` as the first engine holds it. This and the
    /// inspectors below settle the queue first.
    pub fn raster_reg(&mut self, r: u32) -> u32 {
        self.settle(None);
        self.sh.inner.lock().core.back.reg(r)
    }

    /// The pixel at (x, y) of `buffer`, as stored, current from the GPU.
    pub fn pixel(&mut self, buffer: &Buffer, x: u32, y: u32) -> u64 {
        let r = Rect::new(x as i32, y as i32, x as i32, y as i32);
        let _ = self.observe(&Region { buffer: *buffer, rect: r });
        self.sh.inner.lock().core.back.pixel(buffer, x, y)
    }

    /// Pixels `x0..x1` of row `y` of `buffer`, one per word of `out` (which
    /// must hold `x1 - x0`): the low 32 bits of a wide pixel, the 9 bits of an
    /// overlay one. One observe covers the row, so a scan-out reading a
    /// screen takes a call a row, not one a pixel. A range outside the planes
    /// is a refusal and reads nothing.
    pub fn read_row(&mut self, buffer: &Buffer, y: u32, x0: u32, x1: u32, out: &mut [u32]) -> Result<(), Fault> {
        if x0 > x1 || x1 as usize > rss::WIDTH || y as usize >= rss::HEIGHT || out.len() < (x1 - x0) as usize {
            return Err(Fault(format!("read_row: pixels {x0}..{x1} of row {y} into {} words", out.len())));
        }
        if x0 == x1 {
            return Ok(());
        }
        self.observe(&Region { buffer: *buffer, rect: Rect::new(x0 as i32, y as i32, x1 as i32 - 1, y as i32) })?;
        let inner = self.sh.inner.lock();
        let mut row = [0u32; rss::WIDTH];
        inner.core.back.planes().mem.read_row(buffer, y, &mut row[..x1 as usize]);
        out[..(x1 - x0) as usize].copy_from_slice(&row[x0 as usize..x1 as usize]);
        Ok(())
    }

    /// Attach (or with None detach) the shell's device on the display control bus: an option board such as
    /// the Digital Video board answers the slots the engine has no device for (`display::BusSlave`).
    pub fn set_display_bus(&self, slave: Option<Box<dyn display::BusSlave>>) {
        self.display.lock().set_slave(slave);
    }

    /// The hardware cursor as the VC3's registers hold it: the glyph's top-left pixel in screen pixels from the top left
    /// (the position registers less 31, signed), and whether it is shown (cursor control bits 0 and 1 both set).
    /// The position is given whether or not it is shown. Does not wait for queued work.
    pub fn cursor_position(&self) -> (i32, i32, bool) {
        self.display.lock().dcb().vc3.cursor_position()
    }

    /// The VC3's display control register (0x1e) as the guest last wrote it: bit 0 enables the retrace interrupt,
    /// bit 2 the display. A shell that raises the retrace interrupt reads the enable here.
    pub fn vc3_control(&self) -> u32 {
        self.display.lock().dcb().vc3.control() as u32
    }

    /// The 24 colour planes of the displayed buffer at display (`x`, `y`), `y` = 0 the top row. For tests.
    pub fn fb_pixel(&mut self, x: usize, y: usize) -> u32 {
        let (_, h) = self.display.lock().size();
        let (buffer, row) = {
            let inner = self.sh.inner.lock();
            let display = self.display.lock();
            let (main, _) = super::frame::scanout_buffers(&inner.core.back.rss[0], display.dcb());
            (main, h - 1 - y as u32)
        };
        let _ = self.observe(&Region { buffer, rect: Rect::new(x as i32, row as i32, x as i32, row as i32) });
        self.sh.inner.lock().core.back.pixel(&buffer, x as u32, row) as u32 & 0xFF_FFFF
    }

    /// The first raster engine, for the whole-board tests that read its planes once the queue is done.
    /// Valid until `reset`; reading through it while work is queued is a race.
    #[doc(hidden)]
    pub fn first_raster_engine(&mut self) -> *mut Rss {
        &mut *self.sh.inner.lock().core.back.rss[0] as *mut Rss
    }

    /// A hash of pixel memory, the clip IDs and the texture memory: equal
    /// for runs that stored the same words.
    pub fn state_hash(&mut self) -> Result<u64, Fault> {
        self.settle(None);
        let mut inner = self.sh.inner.lock();
        inner.core.back.sync().map_err(Fault)?;
        let planes = inner.core.back.planes();
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut eat = |w: u64| h = (h ^ w).wrapping_mul(0x0000_0100_0000_01b3);
        planes.mem.words.iter().for_each(|&w| eat(w));
        planes.cid.chunks(8).for_each(|c| eat(u64::from_le_bytes(c.try_into().unwrap())));
        planes.te.snapshot_bytes().map_err(Fault)?.chunks(8).for_each(|c| eat(c.iter().fold(0, |a, &b| a << 8 | b as u64)));
        Ok(h)
    }

    /// How many raster engines draw.
    pub fn raster_engines(&mut self) -> usize {
        self.sh.inner.lock().core.back.engines()
    }

    pub fn stats(&mut self) -> Hq3Stats {
        self.settle(None);
        self.sh.inner.lock().hq.stats
    }

    /// The fingerprint of the HQ microcode written so far (0: none).
    pub fn ucode_fingerprint(&mut self) -> u64 {
        self.settle(None);
        self.sh.inner.lock().core.regs.ucode_hash.get()
    }

    /// Primitives the JIT drew, drew while a shader compiled (interpreter),
    /// and no shader covers.
    #[cfg(feature = "gr4-jit")]
    pub fn jit_counts(&mut self) -> (u64, u64, u64) {
        self.settle(None);
        let inner = self.sh.inner.lock();
        let j = &inner.core.back.rss[0].jit;
        (j.hits, j.misses, j.declined)
    }

    /// The parser's and the GL stand-in's state, as text.
    pub fn describe(&mut self) -> String {
        self.settle(None);
        let inner = self.sh.inner.lock();
        format!("{}\n{}", inner.hq.summary(), inner.hq.gl_summary())
    }

    /// No worker, nothing staged or queued; and, unless `reset` asks, no fault.
    fn need_parked(&self, healthy: bool) -> Result<(), Refused> {
        match self.fault() {
            Some(f) if healthy => return Err(Refused::Faulted(f)),
            _ => {}
        }
        if self.worker.is_some() || !self.stage.is_empty() || !self.is_idle() {
            return Err(Refused::NotQuiesced);
        }
        Ok(())
    }

    fn idle(&self) -> Idle {
        if self.dead() {
            Idle::Faulted
        } else if self.is_idle() {
            Idle::Yes
        } else {
            Idle::Working
        }
    }
}

impl<M: HostMem> BoardEngine for MgrasEngine<M> {
    type Region = Region;

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        if self.dead() {
            return Write::Fault;
        }
        let (hi, lo) = ((val >> 32) as u32, val as u32);
        let words = if bits == 64 { 2 } else { 1 };
        match off {
            host::CFIFO | 0x70084 | host::CFIFO_GL | host::CFIFO_PRIVILEGED | 0x50084 => {
                if !self.room(words) {
                    return Write::Busy;
                }
                let port = if off >= host::STATUS { 0 } else { 1 };
                if bits == 64 {
                    self.stage_op(Op::Word { port, w: hi });
                }
                self.stage_op(Op::Word { port, w: lo });
            }
            host::RASTER..=0x7FFFF => {
                if !self.room(words) {
                    return Write::Busy;
                }
                let r = ((off & 0xFFC) >> 2) as u16;
                let exec = off & 0x1000 != 0;
                if bits == 64 {
                    self.stage_op(Op::Raster { reg: r, val: hi, exec: false });
                    self.stage_op(Op::Raster { reg: r + 1, val: lo, exec });
                } else {
                    self.stage_op(Op::Raster { reg: r, val: lo, exec });
                }
            }
            host::CONTEXT_SWITCH => {
                if !self.room(1) {
                    return Write::Busy;
                }
                self.sh.front.flags.fetch_or(host::FLAG_CONTEXT_SAVED, Ordering::AcqRel);
                self.sh.out.post(Event::Irq);
                self.stage_op(Op::ContextSwitch);
            }
            host::SET_FLAGS | host::SET_FLAGS_PRIVILEGED => {
                self.sh.front.flags.fetch_or(lo, Ordering::AcqRel);
                self.sh.out.post(Event::Irq);
            }
            host::CLEAR_FLAGS | host::CLEAR_FLAGS_PRIVILEGED => {
                self.sh.front.flags.fetch_and(!lo, Ordering::AcqRel);
                self.sh.out.post(Event::Irq);
            }
            host::FLAG_ENABLE_SET => drop(self.sh.front.flag_enable.fetch_or(lo, Ordering::AcqRel)),
            host::FLAG_ENABLE_CLEAR => drop(self.sh.front.flag_enable.fetch_and(!lo, Ordering::AcqRel)),
            host::INTERRUPT_ENABLE_SET | host::INTERRUPT_ENABLE_CLEAR => {
                if off == host::INTERRUPT_ENABLE_SET {
                    self.sh.front.interrupt_enable.fetch_or(lo, Ordering::AcqRel);
                } else {
                    self.sh.front.interrupt_enable.fetch_and(!lo, Ordering::AcqRel);
                }
                self.sh.out.post(Event::Irq);
            }
            0x80000..=0xFFFFF => self.sh.out.post(Event::Note(format!("fast-path command window write at {off:#x}"))),
            host::UCODE..=0x45FFF => {
                if !self.room(1) {
                    return Write::Busy;
                }
                self.stage_op(Op::Ucode { index: ((off - host::UCODE) / 4) as u16, w: lo & 0xFF_FFFF });
            }
            // The display bus is the board's own side bus: it answers at once, whatever the raster side is doing.
            display::DCB_FIRST..=display::DCB_LAST => {
                if self.display.lock().write(off, bits, val) {
                    self.sh.generation.fetch_add(1, Ordering::Release);
                }
            }
            _ => {
                if !self.room(1) {
                    return Write::Busy;
                }
                self.stage_op(Op::Reg { off, val: lo });
            }
        }
        Write::Done
    }

    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        // The command FIFO's port: 0 the user's, 1 the privileged one, or the window offset of
        // either (the shell may name the port as it does for `write`). Anything else is no FIFO.
        let port = match port {
            0 | host::CFIFO | 0x70084 | host::CFIFO_GL => 0u8,
            1 | host::CFIFO_PRIVILEGED | 0x50084 => 1,
            _ => {
                log::warn!("mgras engine: push_words to {port:#x}, which is no command FIFO port; nothing taken");
                return 0;
            }
        };
        if self.dead() {
            return 0;
        }
        let take = if self.room(words.len() as u64) {
            words.len()
        } else {
            (self.cfg.queue_ops as u64).saturating_sub(self.pending_ops()) as usize
        };
        for &w in &words[..take] {
            self.stage_op(Op::Word { port, w });
        }
        take
    }

    fn read(&mut self, off: u32, bits: u32) -> Read {
        if self.dead() {
            return Read::Fault;
        }
        // A guest that polls status with fewer than a full stage staged hands
        // the worker its work here, so it makes progress without a pump.
        self.flush();
        match off {
            host::STATUS | host::FIFOSTATUS | host::GIOSTATUS | host::DMABUSY => return Read::Value(self.status(off) as u64),
            host::SET_FLAGS | host::CLEAR_FLAGS | host::SET_FLAGS_PRIVILEGED | host::CLEAR_FLAGS_PRIVILEGED => return Read::Value(self.flags() as u64),
            host::FLAG_ENABLE_SET | host::FLAG_ENABLE_CLEAR => return Read::Value(self.sh.front.flag_enable.load(Ordering::Acquire) as u64),
            host::INTERRUPT_ENABLE_SET | host::INTERRUPT_ENABLE_CLEAR => return Read::Value(self.sh.front.interrupt_enable.load(Ordering::Acquire) as u64),
            display::DCB_FIRST..=display::DCB_LAST => return Read::Value(self.display.lock().read(off, bits)),
            _ => {}
        }
        if !self.is_idle() {
            return Read::Busy;
        }
        let Some(mut inner) = self.sh.inner.try_lock() else { return Read::Busy };
        match catch_unwind(AssertUnwindSafe(|| {
            hit(Point::Read);
            inner.read(off, bits)
        })) {
            Ok(v) => Read::Value(v),
            Err(p) => {
                drop(inner);
                self.sh.fail(&format!("the engine panicked: {}", panic_text(&*p)));
                Read::Fault
            }
        }
    }

    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        if self.dead() {
            return false;
        }
        if write {
            let queued = matches!(off, host::CFIFO | 0x70084 | host::CFIFO_GL | host::CFIFO_PRIVILEGED | 0x50084 | host::RASTER..=0x7FFFF | host::CONTEXT_SWITCH | host::UCODE..=0x45FFF);
            return queued && self.pending_ops() + if bits == 64 { 2 } else { 1 } > self.cfg.queue_ops as u64;
        }
        let at_once = matches!(off, host::STATUS | host::FIFOSTATUS | host::GIOSTATUS | host::DMABUSY | host::SET_FLAGS | host::CLEAR_FLAGS | host::SET_FLAGS_PRIVILEGED | host::CLEAR_FLAGS_PRIVILEGED | host::FLAG_ENABLE_SET | host::FLAG_ENABLE_CLEAR | host::INTERRUPT_ENABLE_SET | host::INTERRUPT_ENABLE_CLEAR | display::DCB_FIRST..=display::DCB_LAST);
        !at_once && !self.is_idle()
    }

    fn run(&mut self, max_ops: usize) -> usize {
        if self.worker.is_some() || self.dead() {
            return 0;
        }
        self.sh.out.rearm();
        self.run_ops(max_ops)
    }

    fn advance(&mut self, now_ns: u64) -> Pump {
        self.now = now_ns;
        self.flush();
        self.sh.out.rearm();
        Pump { idle: self.idle(), events: self.sh.out.pending(), next_deadline: None, irq_levels: self.irq_levels() }
    }

    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle {
        self.now = now_ns;
        self.settle(Some(Duration::from_nanos(budget_ns)));
        self.idle()
    }

    fn irq_levels(&self) -> u64 {
        self.sh.front.irq_levels()
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        self.sh.out.drain(out, max);
    }

    fn observe(&mut self, region: &Region) -> Result<(), Fault> {
        self.settle(None);
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        let r = region.rect;
        let mut inner = self.sh.inner.lock();
        inner.core.back.observe(&[region.buffer], (r.x0, r.y0, r.x1, r.y1)).map_err(Fault)
    }

    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        if head != 0 {
            return None;
        }
        let (t, generation) = self.display.lock().timing()?;
        Some(HeadTiming { frame_clocks: t.frame_clocks, clock_hz: t.clock_hz, fields: t.fields, lines: t.lines, epoch_ns: -1, generation })
    }

    fn busy_ns(&self) -> u64 {
        self.sh.busy_ns.load(Ordering::Relaxed)
    }

    fn heads(&self) -> u32 {
        1
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        (head == 0).then(|| self.display.lock().size())
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if head != 0 {
            return 0;
        }
        // The count only grows, so the sum moves whenever it does; never 0.
        1 + self.sh.generation.load(Ordering::Acquire)
    }

    /// Does not wait for the queued work, so a shell's refresh on its main thread never waits for a long draw: the
    /// frame is the planes between the worker's batches, and a drawing in progress shows for a frame (`observe`
    /// waits). The staged operations are handed to the worker first.
    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        out.clear();
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        if head != 0 {
            return Ok(None);
        }
        self.flush();
        let mut inner = self.sh.inner.lock();
        let mut display = self.display.lock();
        let back = &mut inner.core.back;
        let (w, h) = display.size();
        out.resize(w as usize * h as usize, 0);
        match back.gpu_frame(display.dcb(), out, w as usize) {
            Some(Ok(_)) => out.iter_mut().for_each(|p| *p = display::rgb(*p)),
            Some(Err(e)) => return Err(Fault(e)),
            None => return Ok(Some(display.compose(&back.rss[0], out))),
        }
        Ok(Some((w, h)))
    }

    #[cfg(feature = "scanout")]
    fn lend_frame(&mut self, head: u32) -> Result<Option<LentFrame>, Fault> {
        if head != 0 || self.gpu.is_none() {
            return Ok(None);
        }
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        self.settle(None);
        let generation = self.frame_generation(0);
        let mut inner = self.sh.inner.lock();
        let display = self.display.lock();
        match inner.core.back.gpu_lend(display.dcb()) {
            Some(Ok((buffer, width, height))) => Ok(Some(LentFrame { width, height, stride: width, words: FrameWords::Abgr, generation, buffer: Arc::new(buffer) })),
            Some(Err(e)) => Err(Fault(e)),
            None => Ok(None),
        }
    }

    /// `dst` is a `wgpu::Buffer`, or an `Arc` of one, on the engine's device.
    #[cfg(feature = "scanout")]
    fn compose_into(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if head != 0 || self.gpu.is_none() {
            return Ok(None);
        }
        let dst = match (dst.downcast_ref::<wgpu::Buffer>(), dst.downcast_ref::<Arc<wgpu::Buffer>>()) {
            (Some(b), _) => b,
            (None, Some(b)) => b.as_ref(),
            (None, None) => return Err(Fault("compose_into: the destination is not a wgpu buffer".into())),
        };
        // The mode's size bounds every frame: a bad destination is an error of the call, with the engine and the device untouched.
        let (w, h) = self.display.lock().size();
        crate::wgpu::Scanout::check_into(w, h, dst, stride_words).map_err(|why| Fault(format!("compose_into: {why}")))?;
        if let Some(f) = self.fault() {
            return Err(Fault(f));
        }
        self.settle(None);
        let mut inner = self.sh.inner.lock();
        let display = self.display.lock();
        let composed = inner.core.back.gpu_compose_into(display.dcb(), dst, stride_words);
        drop((display, inner));
        match composed {
            Some(Ok(size)) => match self.fault() {
                Some(f) => Err(Fault(f)),
                None => Ok(Some(size)),
            },
            Some(Err(e)) => Err(Fault(format!("compose_into: {e}"))),
            None => Ok(None),
        }
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_parked(true)?;
        self.sh.inner.lock().core.back.sync().map_err(Refused::Faulted)
    }

    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        let budget = (budget_ns != u64::MAX).then(|| Duration::from_nanos(budget_ns));
        if !self.settle(budget) && !self.dead() {
            return Err(Refused::Midway("queued work did not finish within the budget".into()));
        }
        self.stop_worker();
        self.run_ops(usize::MAX);
        self.parked = true;
        match self.fault() {
            Some(f) => Err(Refused::Faulted(f)),
            None => Ok(()),
        }
    }

    fn resume(&mut self) {
        if self.parked {
            self.parked = false;
            self.spawn();
        }
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_parked(true)?;
        let board = self.cfg.board;
        let display = self.display.lock();
        self.sh.inner.lock().save(board, &display, sink)
    }

    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        self.need_parked(true)?;
        let board = self.cfg.board;
        let mut display = self.display.lock();
        let loaded = self.sh.inner.lock().load(board, &mut display, src);
        if loaded.is_ok() {
            self.sh.generation.fetch_add(1, Ordering::Release);
        }
        loaded
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn reset(&mut self) -> Result<(), Refused> {
        self.need_parked(false)?;
        let inner = Inner::new(&self.cfg, self.gpu.as_ref(), self.mem.clone(), self.sh.out.clone(), self.sh.front.clone()).map_err(Refused::Faulted)?;
        *self.sh.inner.lock() = inner;
        {
            // The shell's bus device is the shell's, not the board's state: it stays through a reset.
            let mut display = self.display.lock();
            display.reset(self.cfg.bdvers.unwrap_or_else(|| display::default_bdvers(self.cfg.board)), self.cfg.cmap_rev, self.cfg.pp1_revision());
        }
        self.sh.generation.fetch_add(1, Ordering::Release);
        let f = &self.sh.front;
        f.flags.store(0, Ordering::Release);
        f.flag_enable.store(0, Ordering::Release);
        f.interrupt_enable.store(0, Ordering::Release);
        self.sh.out.clear();
        *self.sh.fault.lock() = None;
        self.sh.dead.store(false, Ordering::Release);
        self.sh.out.post(Event::Irq);
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.sh.fault.lock().clone().or_else(|| self.sh.inner.try_lock().and_then(|i| i.core.back.fault()))
    }

    fn fail(&self, why: &str) {
        self.sh.fail(why);
    }
}

impl<M: HostMem> Drop for MgrasEngine<M> {
    fn drop(&mut self) {
        self.stop_worker();
    }
}
