//! The IMPACT engine from the HQ3 command FIFO down: the command parser,
//! the GE11 stand-in, the raster engine, the pixel processor and the
//! texture engine, driven by words and register accesses at the HQ3
//! window's offsets.
//!
//! No host clock is read and no thread is started here. Time is an argument
//! (`advance`); what the board would signal comes out in an outbox
//! (`drain_events`), and a notify hook only tells the caller that something
//! is waiting. In `Mode::Inline` every write runs at once; in
//! `Mode::Queued` writes are held until `advance` runs them, and a read that
//! depends on earlier writes is answered through the outbox after they have
//! run.
//!
//! One raster engine draws every scanline, as the GPU path of iris's board
//! does: the two engines of a Maximum IMPACT cannot be stepped one after the
//! other because they wait on each other at a texture write.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use super::back::ImpactBack;
use super::hostmem::HostMem;
use super::hq3::{host, Hq3Board, Hq3Engine, Hq3Regs, Hq3Sink};
use super::pixmem::Buffer;
use super::plain::boxed_zeroed;
use super::quirks::Hq4Quirks;
use super::rss::{Entry, Rss};
use crate::display::Rect;
use crate::wgpu::Gpu;

/// The boards that share this engine; they differ in the geometry engine
/// count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Board {
    Solid,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every write runs before the call returns. No queue.
    Inline,
    /// Writes queue until `advance`.
    Queued,
}

pub struct Config {
    pub board: Board,
    /// The Octane's HQ4 front: its microcode's GE layout and read order.
    pub octane: bool,
    pub mode: Mode,
    /// `rss_jit::MODE_*`; None leaves the crate's default.
    #[cfg(feature = "gr4-jit")]
    pub jit: Option<u32>,
}

/// What the board would signal, in the order it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The HQ flags changed (`flags()`): the shell derives its interrupt
    /// levels from them.
    Flags,
    /// Pixels were drawn: see `take_damage`.
    Damage,
    /// Something not modelled yet, once per distinct text.
    Note(String),
    /// The answer to a read that had to wait behind the stream.
    ReadDone { ticket: u64, value: u64 },
}

/// A read's answer: now, or in the outbox behind the writes before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Value(u64),
    Queued(u64),
}

#[derive(Clone, Copy)]
enum Op {
    Word { port: u8, w: u32 },
    Raster { reg: u16, val: u32, exec: bool },
    ContextSwitch,
    Read { off: u32, bits: u8, ticket: u64 },
}

pub struct Engine {
    hq: Hq3Engine,
    core: Core,
    mode: Mode,
    queue: VecDeque<Op>,
    queued_words: u32,
    next_ticket: u64,
    notify: Option<Box<dyn Fn() + Send + Sync>>,
    notified: bool,
    now: u64,
}

/// Everything but the command parser: the half the parser drives through
/// `Hq3Sink`.
struct Core {
    back: ImpactBack,
    regs: Box<Hq3Regs>,
    flags: u32,
    mem: Option<Arc<dyn HostMem>>,
    events: Vec<Event>,
    notes: HashSet<String>,
    damage: Option<Rect>,
    /// Each buffer drawn into so far, and the box drawn in it.
    touched: Vec<(Buffer, Rect)>,
    flags_pending: bool,
    damage_pending: bool,
}

impl Engine {
    pub fn new(cfg: Config, gpu: Option<&Arc<Gpu>>) -> Result<Engine, String> {
        let mut hq = Hq3Engine::new(if cfg.board == Board::Max { 2 } else { 1 });
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
        Ok(Engine {
            hq,
            core: Core {
                back,
                regs: boxed_zeroed::<Hq3Regs>(),
                flags: 0,
                mem: None,
                events: Vec::new(),
                notes: HashSet::new(),
                damage: None,
                touched: Vec::new(),
                flags_pending: false,
                damage_pending: false,
            },
            mode: cfg.mode,
            queue: VecDeque::new(),
            queued_words: 0,
            next_ticket: 1,
            notify: None,
            notified: false,
            now: 0,
        })
    }

    /// The memory the board's DMA reads and writes. Called from the thread
    /// that calls `write` (inline) or `advance` (queued).
    pub fn set_mem(&mut self, mem: Arc<dyn HostMem>) {
        self.core.mem = Some(mem);
    }

    /// Called when work is queued or an event is waiting and the caller has
    /// not yet been told. It must only schedule: it may not call back in.
    pub fn set_notify(&mut self, f: Box<dyn Fn() + Send + Sync>) {
        self.notify = Some(f);
    }

    /// The time the engine last saw, ns. The engine reads no clock.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Tell the caller once per batch of waiting work or events.
    fn wake(&mut self) {
        if !self.notified {
            self.notified = true;
            if let Some(n) = &self.notify {
                n();
            }
        }
    }

    #[inline]
    fn submit(&mut self, op: Op) {
        match self.mode {
            Mode::Inline => {
                self.exec(op);
                if !self.core.events.is_empty() {
                    self.wake();
                }
            }
            Mode::Queued => {
                if matches!(op, Op::Word { .. }) {
                    self.queued_words += 1;
                }
                self.queue.push_back(op);
                self.wake();
            }
        }
    }

    fn exec(&mut self, op: Op) {
        match op {
            Op::Word { port, w } => self.hq.push(port as usize, w, &mut self.core),
            Op::Raster { reg, val, exec } => self.core.rss_write(reg as u32, val, exec),
            Op::ContextSwitch => self.hq.begin_context_switch(&mut self.core),
            Op::Read { off, bits, ticket } => {
                let value = self.read_now(off, bits as u32);
                self.core.events.push(Event::ReadDone { ticket, value });
            }
        }
    }

    /// Run queued writes: up to `max_ops` of them, and `now` is the time the
    /// caller says it is. Returns how many ran.
    pub fn advance(&mut self, now: u64, max_ops: usize) -> usize {
        self.now = now;
        let mut n = 0;
        while n < max_ops {
            let Some(op) = self.queue.pop_front() else { break };
            if matches!(op, Op::Word { .. }) {
                self.queued_words -= 1;
            }
            self.exec(op);
            n += 1;
        }
        self.core.back.rss[0].gpu_flush();
        if !self.core.events.is_empty() {
            self.wake();
        }
        n
    }

    /// True when nothing is queued. Never waits.
    pub fn idle(&self) -> bool {
        self.queue.is_empty()
    }

    /// A batch of command words into FIFO port `port` (0 user, 1 privileged).
    pub fn push_words(&mut self, port: u8, words: &[u32]) {
        for &w in words {
            self.submit(Op::Word { port, w });
        }
    }

    /// A write at HQ3 window offset `off`, `bits` wide (the offsets of
    /// `hq3::host`).
    pub fn write(&mut self, off: u32, bits: u32, val: u64) {
        let (hi, lo) = ((val >> 32) as u32, val as u32);
        match off {
            host::CFIFO | 0x70084 | host::CFIFO_GL | host::CFIFO_PRIVILEGED | 0x50084 => {
                let port = if off >= host::STATUS { 0 } else { 1 };
                if bits == 64 {
                    self.submit(Op::Word { port, w: hi });
                }
                self.submit(Op::Word { port, w: lo });
            }
            host::RASTER..=0x7FFFF => {
                let r = ((off & 0xFFC) >> 2) as u16;
                let exec = off & 0x1000 != 0;
                if bits == 64 {
                    self.submit(Op::Raster { reg: r, val: hi, exec: false });
                    self.submit(Op::Raster { reg: r + 1, val: lo, exec });
                } else {
                    self.submit(Op::Raster { reg: r, val: lo, exec });
                }
            }
            host::CONTEXT_SWITCH => {
                self.core.set_flags(host::FLAG_CONTEXT_SAVED);
                self.submit(Op::ContextSwitch);
            }
            host::SET_FLAGS | host::SET_FLAGS_PRIVILEGED => self.core.set_flags(lo),
            host::CLEAR_FLAGS | host::CLEAR_FLAGS_PRIVILEGED => {
                self.core.flags &= !lo;
                self.core.flag_event();
            }
            host::UCODE..=0x45FFF => {
                let i = ((off - host::UCODE) / 4) as usize;
                self.core.regs.ucode[i] = lo & 0xFF_FFFF;
                self.core.regs.ucode_hash.word(i, lo & 0xFF_FFFF);
            }
            host::FLAG_ENABLE_SET => self.core.regs.flag_enable |= lo,
            host::FLAG_ENABLE_CLEAR => self.core.regs.flag_enable &= !lo,
            host::INTERRUPT_ENABLE_SET => self.core.regs.interrupt_enable |= lo,
            host::INTERRUPT_ENABLE_CLEAR => self.core.regs.interrupt_enable &= !lo,
            0x80000..=0xFFFFF => self.core.note(format!("fast-path command window write at {off:#x}")),
            _ => self.core.regs.regs.insert(off, lo),
        }
    }

    /// A read at HQ3 window offset `off`. Status is answered from state and
    /// never waits. What depends on the writes before it (raster registers,
    /// pixel reads, the geometry engine's answers) is answered at once when
    /// nothing is queued, and otherwise through the outbox.
    pub fn read(&mut self, off: u32, bits: u32) -> Read {
        match off {
            host::STATUS | host::FIFOSTATUS | host::GIOSTATUS | host::DMABUSY => {
                return Read::Value(self.status(off) as u64)
            }
            host::SET_FLAGS | host::CLEAR_FLAGS | host::SET_FLAGS_PRIVILEGED | host::CLEAR_FLAGS_PRIVILEGED => {
                return Read::Value(self.core.flags as u64)
            }
            _ => {}
        }
        if self.queue.is_empty() {
            return Read::Value(self.read_now(off, bits));
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.queue.push_back(Op::Read { off, bits: bits as u8, ticket });
        Read::Queued(ticket)
    }

    fn status(&self, off: u32) -> u32 {
        let idle = self.queue.is_empty();
        match off {
            host::STATUS => {
                (if idle { host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE } else { 0 })
                    | host::STATUS_FIXED
                    | host::STATUS_CFIFO_LOW
            }
            host::FIFOSTATUS => self.queued_words.min(0x7E),
            host::DMABUSY => 0,
            _ => 0,
        }
    }

    fn read_now(&mut self, off: u32, bits: u32) -> u64 {
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
            host::FLAG_ENABLE_SET | host::FLAG_ENABLE_CLEAR => self.core.regs.flag_enable as u64,
            host::INTERRUPT_ENABLE_SET | host::INTERRUPT_ENABLE_CLEAR => self.core.regs.interrupt_enable as u64,
            _ => self.core.regs.regs.get(off) as u64,
        }
    }

    /// The HQ flags as the board holds them.
    pub fn flags(&self) -> u32 {
        self.core.flags
    }

    /// The fingerprint of the HQ microcode written so far (0: none).
    pub fn ucode_fingerprint(&self) -> u64 {
        self.core.regs.ucode_hash.get()
    }

    /// Move the outbox's events into `out`, oldest first.
    pub fn drain_events(&mut self, out: &mut Vec<Event>) {
        self.core.flags_pending = false;
        self.core.damage_pending = false;
        self.notified = false;
        out.append(&mut self.core.events);
    }

    /// The box drawn since the last call.
    pub fn take_damage(&mut self) -> Option<Rect> {
        self.core.damage.take()
    }

    /// Submit GPU work and bring the CPU's planes current. A no-op on the
    /// CPU.
    pub fn sync(&mut self) -> Result<(), String> {
        self.core.back.sync()
    }

    pub fn pixel(&self, buffer: &Buffer, x: u32, y: u32) -> u64 {
        self.core.back.pixel(buffer, x, y)
    }

    pub fn raster_reg(&self, r: u32) -> u32 {
        self.core.back.reg(r)
    }

    pub fn fault(&self) -> Option<String> {
        self.core.back.fault()
    }

    pub fn stats(&self) -> super::hq3::Hq3Stats {
        self.hq.stats
    }

    /// Primitives the JIT drew, drew while a shader compiled (interpreter),
    /// and no shader covers.
    #[cfg(feature = "gr4-jit")]
    pub fn jit_counts(&self) -> (u64, u64, u64) {
        let j = &self.core.back.rss[0].jit;
        (j.hits, j.misses, j.declined)
    }

    /// The buffers drawn into so far, each with the box drawn in it.
    pub fn touched(&self) -> &[(Buffer, Rect)] {
        &self.core.touched
    }

    /// The parser's and the GL stand-in's state, as text.
    pub fn describe(&self) -> String {
        format!("{}\n{}", self.hq.summary(), self.hq.gl_summary())
    }
}

impl Core {
    fn note(&mut self, what: String) {
        if self.notes.insert(what.clone()) {
            self.events.push(Event::Note(what));
        }
    }

    fn flag_event(&mut self) {
        if !self.flags_pending {
            self.flags_pending = true;
            self.events.push(Event::Flags);
        }
    }

    fn set_flags(&mut self, bits: u32) {
        self.flags |= bits;
        self.flag_event();
    }

    /// One raster register write, as the raster engine's thread makes it in
    /// iris's board: the destination RE gate, then the write.
    #[inline]
    fn rss_write(&mut self, r: u32, val: u32, exec: bool) {
        if self.back.run(Entry::Write { reg: r, val, exec }) {
            self.drawn();
        }
    }

    fn drawn(&mut self) {
        let mut rect: Option<Rect> = None;
        let touched = &mut self.touched;
        for rss in &self.back.rss {
            rss.drawn(|b, _, r| {
                rect = Some(rect.map_or(r, |u| union(u, r)));
                match touched.iter_mut().find(|(t, _)| t.ptr == b.ptr && t.kind == b.kind) {
                    Some((_, u)) => *u = union(*u, r),
                    None => touched.push((b, r)),
                }
            });
        }
        if let Some(r) = rect {
            self.damage = Some(self.damage.map_or(r, |u| union(u, r)));
            if !self.damage_pending {
                self.damage_pending = true;
                self.events.push(Event::Damage);
            }
        }
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) }
}

impl Hq3Sink for Core {
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
        Core::set_flags(self, bits);
    }
    fn take_flags(&mut self, mask: u32) -> u32 {
        let was = self.flags & mask;
        self.flags &= !mask;
        self.flag_event();
        was
    }
    fn sys_mem(&mut self) -> Option<Arc<dyn HostMem>> {
        self.mem.clone()
    }
    fn note(&mut self, what: String) {
        Core::note(self, what);
    }
}
