//! One KONA pipe's front end, token layer and drawing behind `sgi_engine::BoardEngine`.
//!
//! No thread of its own: the shell calls `write` and `read` for the bus, `run` or `wait_idle` for the
//! command stream and `frame` at each retrace it chooses. Interrupt packets and credits go to the
//! event queue the shell drains; host memory is the shell's `HostMem`. Iris's front end is mounted
//! unchanged. This file is iris's board routing without its ring and thread, plus Hub credits.
//!
//! The pipe's stream waits at a swap until the retrace, so `run` stops there and `Idle` says
//! `Stalled(Frame)` until the shell calls `frame`. Drawing runs on the pipe's tile workers, or on the
//! caller's thread when the configuration has none.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sgi_engine::{BoardEngine, Chunk, Event, Fault, HeadTiming, HostMem, Idle, Interrupt, Outbox, Pump, Read, Rect, Refused, Source, StallOn, StateSink, StateSource, Swap, Write};

use super::back::snapshot::Layout;
use super::back::KonaBackend;
use super::defs::*;
use super::flash::{Flash, FlashSink};
use super::front::{Front, Host, InterruptPacket};
use super::handle::DisplayHandle;
use super::ident::KonaConfig;
use super::pipe::{Core, LoadSection};
use super::regs::{classify, Access};
use super::stream::{self, Stream};
use crate::chunk_store::CHUNK_SIZE;
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BUS_ERR, BUS_OK};
use crate::SharedGpu;

/// The version of the state `save` writes. Version 1 held only a stream between commands;
/// 2 adds the commands a guest has part way and the credits they owe; 3 the swaps latched for
/// the next frame; 4 keeps the credits held and parked by source. A load takes all four.
pub const STATE_VERSION: u32 = 4;

/// Stores `quiesce` and `wait_idle` run between looks at the clock.
const SLICE: usize = 1 << 12;

/// Host memory as the pipe's DMA reaches it: physical addresses before the fabric adds the widget.
/// The pipe's DMA runs on the caller's thread inside a call, so it uses the `bus_*` path.
struct Mem<M: HostMem>(M);

impl<M: HostMem> BusDevice for Mem<M> {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> u32 {
        if self.0.bus_read(addr, buf, Swap::None).is_ok() { BUS_OK } else { BUS_ERR }
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> u32 {
        if self.0.bus_write(addr, data, Swap::None).is_ok() { BUS_OK } else { BUS_ERR }
    }
}

/// The shell's event queue, and where the pipe's interrupt packets go. `Outbox` keeps one packet for
/// each line, so a guest that raises a vector faster than the shell drains adds nothing.
struct Events {
    out: Outbox,
    /// The widget the host's packets go to (the DMA widget the driver programmed).
    host_widget: Arc<AtomicU8>,
}

impl Events {
    fn post(&self, sender: u8, widget: u8, addr: u64, vector: u8, level: bool) {
        self.out.post(Event::Interrupt(Interrupt { sender, widget, addr, vector, level }));
    }
}

impl InterruptController for Events {
    fn set_line(&self, _line: u32, _level: bool) {}

    fn packet(&self, source: u8, address: u64, line: u32, level: bool) -> bool {
        self.post(source, self.host_widget.load(Relaxed), address, line as u8, level);
        true
    }

    fn foreign_packet(&self, source: u8, widget: u8, address: u64, line: u32, level: bool) -> bool {
        self.post(source, widget, address, line as u8, level);
        true
    }
}

/// A composed frame the GPU holds: the shell's scan-out on the engine's device reads it in place.
#[cfg(feature = "wgpu")]
pub struct LentFrame {
    pub buffer: wgpu::Buffer,
    pub width: u32,
    pub height: u32,
}

/// The flash a pipe starts with and where its programmed bytes go: the shell's drive.
pub struct FlashDrive {
    /// Exactly `FLASH_SIZE` bytes.
    pub image: Vec<u8>,
    pub sink: Box<dyn FlashSink>,
}

/// What a pipe is built with.
pub struct Options {
    /// The pixel store and the drawing on the GPU, when the shell has one.
    pub gpu: Option<SharedGpu>,
    /// The most stores the command stream holds before `write` answers busy.
    pub queue: usize,
    /// A flash held on a drive; None is a new flash that outlasts nothing.
    pub flash: Option<FlashDrive>,
    /// Told the widget the pipe's DMA goes to, now and at each change the guest makes.
    pub dma_widget: Option<Box<dyn Fn(u8) + Send>>,
}

impl Default for Options {
    fn default() -> Self {
        Options { gpu: None, queue: 1 << 16, flash: None, dma_widget: None }
    }
}

/// A box of pixels in one word position of the store, in the store's own coordinates.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub word: usize,
    pub rect: Rect,
}

/// One store waiting in the stream, with who sent it.
struct Stored {
    off: u32,
    val: u32,
    src: Source,
    /// The store's credit goes back with this word (the last of a store).
    counted: bool,
}

/// Credits owed to Hubs, by the source whose store earned them: a charged store's credit goes back
/// once the pipe has taken its words, and the credits of a packet still arriving wait for its last
/// word, then each goes to the source that sent the store it came from, not all to the last word's
/// sender. While the driver has parked the credit target they wait in `parked`, again by source.
/// Evidence, from the KONA manual page's "Credits and flow control" ratings: the IRIX kernel charges one
/// credit in the storing processor's interface per store and waits for that count, a *software
/// expectation*, so a credit goes back to the Hub that sent its store; that the pipe returns credits as its
/// front end takes words is *inferred*, and so is holding a packet's credits to its last word; how the pipe
/// holds credits while the target is parked is *unknown*, so returning them to their own Hub is the model's
/// reading.
#[derive(Default)]
struct Credits {
    held: BTreeMap<u32, u32>,
    parked: BTreeMap<u32, u32>,
    /// The sender of the last store, who a settle with no store of its own pays.
    last: u32,
}

/// The most sources a state may name: a shell has a handful of Hubs.
const MAX_CREDIT_SOURCES: usize = 64;

impl Credits {
    /// The state of version 4: both maps, each as a list of (source, count) pairs, then `last`. The
    /// by-source split is inferred, not measured (see `Credits`); a state of version 3 or older held one
    /// count each and loads as owed to the last sender.
    fn visit<S: Stream>(&mut self, s: &mut S) {
        for map in [&mut self.held, &mut self.parked] {
            let mut flat: Vec<u32> = map.iter().flat_map(|(&k, &v)| [k, v]).collect();
            s.vec_u32(&mut flat);
            if s.loading() {
                *map = flat.chunks_exact(2).take(MAX_CREDIT_SOURCES).map(|c| (c[0], c[1])).collect();
            }
        }
        s.u32(&mut self.last);
    }

    /// The state of versions 2 and 3: one held and one parked count for everyone, which the old rule
    /// paid to the last sender, so they are owed to it now.
    fn visit_scalar<S: Stream>(&mut self, s: &mut S) {
        let (mut held, mut parked) = (0u32, 0u32);
        s.u32(&mut held);
        s.u32(&mut parked);
        s.u32(&mut self.last);
        if s.loading() {
            self.held = (held != 0).then_some((self.last, held)).into_iter().collect();
            self.parked = (parked != 0).then_some((self.last, parked)).into_iter().collect();
        }
    }
}

/// The state's `meta` chunk: the version, the length of the `state` blob that follows it and the
/// bulk state's layout.
#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    state_len: u32,
    bulk: Layout,
}

pub struct KonaEngine<M: HostMem> {
    host: Arc<Host>,
    core: Core,
    back: KonaBackend,
    stream: VecDeque<Stored>,
    cap: usize,
    events: Arc<Events>,
    credits: Credits,
    pixels: super::back::display::Pixels,
    source: Source,
    parked: bool,
    fault: Arc<Mutex<Option<String>>>,
    /// The widget the shell was last told, and who tells it.
    widget: (Option<u8>, Option<Box<dyn Fn(u8) + Send>>),
    /// Moves with every store, push, frame, load and reset (`frame_generation`).
    activity: u64,
    _mem: std::marker::PhantomData<M>,
}

impl<M: HostMem> KonaEngine<M> {
    /// A pipe of configuration `cfg` reading and writing host memory through `mem`. `notify` runs when
    /// work or an event waits for the shell's pump, from any thread, and may only schedule it.
    pub fn new(cfg: KonaConfig, mem: M, opts: Options, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Result<Self, String> {
        let mut back = KonaBackend::with_gpu(cfg, opts.gpu)?;
        let pixels = back.pixels();
        let mut front = Front::new(&cfg);
        front.probe_value = cfg.probe;
        let events = Arc::new(Events { out: Outbox::new(notify), host_widget: front.dma_widget_handle() });
        let host = Arc::new(Host::new(front, events.clone(), Arc::new(Mem(mem))));
        back.attach_manual(DisplayHandle::new(&host));
        back.latch_swaps(true);
        let flash = match opts.flash {
            Some(d) => Flash::with_sink(&d.image, d.sink)?,
            None => Flash::fresh(),
        };
        let mut engine = KonaEngine {
            host,
            core: Core::new(cfg, flash),
            back,
            stream: VecDeque::new(),
            cap: opts.queue,
            events,
            credits: Credits::default(),
            pixels,
            source: Source::default(),
            parked: false,
            fault: Arc::new(Mutex::new(None)),
            widget: (None, opts.dma_widget),
            activity: 0,
            _mem: std::marker::PhantomData,
        };
        engine.report_widget();
        Ok(engine)
    }

    /// Tell the shell the widget the pipe's DMA goes to, if it is not the one it was last told.
    fn report_widget(&mut self) {
        let now = self.events.host_widget.load(Relaxed);
        if self.widget.0 != Some(now) {
            self.widget.0 = Some(now);
            if let Some(f) = &self.widget.1 {
                f(now);
            }
        }
    }

    /// What a GPU unit's device-loss callback calls once this engine exists (`GpuLoss::attach`): post
    /// `GpuLost`, then fault the engine, which tells the shell.
    pub fn gpu_lost_handler(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let (fault, events) = (self.fault.clone(), self.events.clone());
        move |why| {
            events.out.post(Event::GpuLost(why.to_string()));
            let mut f = fault.lock();
            if f.is_none() {
                *f = Some(why.to_string());
                events.out.post(Event::DeviceFault(why.to_string()));
            }
        }
    }

    /// The device fault, latched or not yet noticed: what `fail` was told, a failed command stream or
    /// a panicked tile worker.
    fn fault_now(&self) -> Option<String> {
        let poisoned = self.back.is_poisoned().then(|| "a tile worker panicked; the pipe is dead".to_string());
        self.fault.lock().clone().or_else(|| self.core.failed.clone()).or(poisoned)
    }

    fn dead(&self) -> bool {
        self.fault.lock().is_some() || self.core.failed.is_some() || self.back.is_poisoned()
    }

    /// Latch a fault the pipe found itself, and tell the shell once.
    fn notice_fault(&self) {
        if self.dead() && self.fault.lock().is_none() {
            if let Some(f) = self.fault_now() {
                self.fail(&f);
            }
        }
    }

    /// The stream holds at a swap until the retrace.
    fn stalled(&self) -> bool {
        !self.stream.is_empty() && self.back.swaps_waiting()
    }

    fn idle(&self) -> Idle {
        if self.dead() {
            Idle::Faulted
        } else if self.stream.is_empty() {
            Idle::Yes
        } else if self.stalled() {
            Idle::Stalled(StallOn::Frame)
        } else {
            Idle::Working
        }
    }

    /// The words of a store of `bits` at `off`, as (offset, value) pairs. An 8-byte store to the
    /// command page is two words, high first, at the same offset; any other is two words at `off` and
    /// `off + 4`. None for a width the registers do not have.
    fn words(off: u32, bits: u32, val: u64) -> Option<([(u32, u32); 2], usize)> {
        match bits {
            32 => Some(([(off, val as u32), (0, 0)], 1)),
            64 => {
                let second = if off == COMMAND_PAGE { off } else { off.wrapping_add(4) };
                Some(([(off, (val >> 32) as u32), (second, val as u32)], 2))
            }
            _ => None,
        }
    }

    fn queues(words: &[(u32, u32)]) -> usize {
        words.iter().filter(|&&(o, _)| classify(o, true) != Access::Now).count()
    }

    /// Whether a read of `off` waits for the stream.
    fn read_waits(&self, off: u32) -> bool {
        classify(off, false) != Access::Now && !self.stream.is_empty()
    }

    /// One word of a store: a register the stream does not own takes it now, the rest queue.
    fn store(&mut self, off: u32, val: u32, counted: bool) {
        if classify(off, true) == Access::Now {
            self.write_now(off, val);
            if counted {
                self.settle(true, self.core.packets, self.source.id);
            }
        } else {
            if self.stream.is_empty() {
                self.events.out.wake();
            }
            self.stream.push_back(Stored { off, val, src: self.source, counted });
        }
    }

    fn read32(&mut self, off: u32) -> u32 {
        match classify(off, false) {
            Access::Now => self.read_now(off),
            _ => self.core.read_wait(&self.host.front, off),
        }
    }

    fn read_now(&mut self, off: u32) -> u32 {
        let pending = !self.stream.is_empty();
        match off {
            INT_CAUSE if self.core.reasons_waiting() => self.core.reason_drain(&self.host),
            ACTIVITY => return if self.host.activity.load(Relaxed) || pending { ACTIVITY_BUS } else { 0 },
            _ => {}
        }
        // The FIFO's fill is a word count in bits 10:1. It reads full while a display list runs.
        let credit = if self.host.list_running.load(Relaxed) {
            CREDIT_MASK
        } else {
            ((self.core.held_words() + self.stream.len() as u32) << 1).min(CREDIT_MASK)
        };
        self.host.front.lock().read(off, credit).unwrap_or(0)
    }

    fn write_now(&mut self, off: u32, value: u32) {
        let host = self.host.clone();
        if off == ACTIVITY {
            if value & ACTIVITY_BUS != 0 {
                host.activity.store(false, Relaxed);
            }
            return;
        }
        let mut packet: Option<InterruptPacket> = None;
        let idle = off == INT_ENABLE && self.stream.is_empty();
        let parked_before = !self.credits.parked.is_empty();
        host.front.lock().write(off, value, idle, &mut packet);
        if let Some(p) = packet {
            host.deliver(p);
        }
        if off == LOW_WINDOW_WRITE && value & 1 == 0 && parked_before {
            for (hub, n) in std::mem::take(&mut self.credits.parked) {
                self.give(hub, n);
            }
        }
        if off == INT_CAUSE && self.core.reasons_waiting() {
            self.core.reason_drain(&host);
        }
        self.report_widget();
    }

    /// The Hub's accounting after the pipe took a store's words (`before`: the packets completed
    /// before it). `hub` is the store's source id; 0 is a store with no sender.
    fn settle(&mut self, counted: bool, before: u64, hub: u32) {
        let open = self.core.held_words() != 0;
        let mut owed = if !open {
            // Nothing holds credits back: everything held goes, each to its source, and this store's own with it.
            let mut owed = std::mem::take(&mut self.credits.held);
            *owed.entry(hub).or_default() += counted as u32;
            owed
        } else if self.core.packets != before {
            // This store completed a packet: the held credits go to the sources that earned them, and this
            // store's own starts the next packet's hold.
            let owed = std::mem::replace(&mut self.credits.held, counted.then_some((hub, 1)).into_iter().collect());
            owed
        } else {
            if counted {
                *self.credits.held.entry(hub).or_default() += 1;
            }
            return;
        };
        owed.retain(|_, n| *n != 0);
        for (hub, n) in owed {
            self.give(hub, n);
        }
    }

    /// Return `credits` to `hub`, unless the driver has parked the credit target, when they wait for it, by source.
    fn give(&mut self, hub: u32, credits: u32) {
        if credits == 0 {
            return;
        }
        if self.host.front.lock().low_window_write & 1 != 0 {
            *self.credits.parked.entry(hub).or_default() += credits;
            return;
        }
        self.events.out.post(Event::Credits { source: hub, retired: credits });
    }

    /// Run the stream, in order, until it is empty, waits at a swap, or `max` stores have run.
    fn run_stream(&mut self, max: usize) -> usize {
        let host = self.host.clone();
        let mut ran = 0;
        while ran < max && !self.back.swaps_waiting() {
            let Some(s) = self.stream.pop_front() else { break };
            ran += 1;
            let before = self.core.packets;
            match classify(s.off, true) {
                Access::Waits => self.core.write_wait(&host, &mut self.back, s.off, s.val),
                _ => self.core.stream_store(&host, &mut self.back, s.off, s.val),
            }
            if s.src.charged || s.src.id != 0 {
                self.credits.last = s.src.id;
            }
            self.settle(s.counted, before, s.src.id);
        }
        self.core.reason_drain(&host);
        if ran != 0 && self.stream.is_empty() {
            // Credits the framers held while the stores were arriving go back once nothing holds them.
            self.settle(false, self.core.packets, self.credits.last);
            host.with_front(|f| ((), f.lowater()));
        }
        self.activity += (ran != 0) as u64;
        self.notice_fault();
        self.report_widget();
        ran
    }

    fn need_parked(&self) -> Result<(), Refused> {
        if let Some(f) = self.fault_now() {
            return Err(Refused::Faulted(f));
        }
        if !self.parked || !self.stream.is_empty() {
            return Err(Refused::NotQuiesced);
        }
        Ok(())
    }

    // ----- what a shell reads off the board --------------------------------

    /// Word `w` of the pixel at (x, y) as the store holds it, after the draws behind it have run.
    pub fn pixel(&mut self, x: u32, y: u32, w: usize) -> u32 {
        self.back.drain();
        self.pixels.word(x, y, w)
    }

    /// The frame the display would show now: `width * height` words from the top, `0xAABBGGRR`.
    pub fn compose(&mut self) -> (u32, u32, Vec<u32>) {
        self.back.drain();
        let (w, h) = self.core.managed;
        (w, h, self.pixels.frame())
    }

    /// The frame the display would show, composed on the GPU and left there for a scan-out on the
    /// same device to take: no readback. None when the pipe has no GPU scan-out. The words are
    /// 0xAABBGGRR, rows from the top, and the buffer is rewritten by the next call.
    #[cfg(feature = "wgpu")]
    pub fn lend_frame(&mut self) -> Result<Option<LentFrame>, String> {
        if let Some(f) = self.fault_now() {
            return Err(f);
        }
        let lent = self.back.lend_frame()?;
        Ok(lent.map(|(buffer, width, height)| LentFrame { buffer, width, height }))
    }

    /// A Crossbow link reset reached the widget: its crosstalk interface registers (widget control, interrupt
    /// destination and vectors) go back to their reset values, which the kernel's adaptor reset rewrites after
    /// it; the map RAM, the interrupt enable and every other register of the pipe stay (`Front::link_reset`,
    /// inferred; the KONA manual page documents it). A shell calls this for the link reset instead of a power-on.
    pub fn link_reset(&mut self) {
        self.host.front.lock().link_reset();
        self.activity += 1;
        self.report_widget();
    }

    /// The sections of the last completed firmware load.
    pub fn load_record(&self) -> &[LoadSection] {
        self.core.load_record()
    }

    /// Stores waiting in the command stream.
    pub fn queued(&self) -> usize {
        self.stream.len()
    }

    /// A swap waits for its retrace.
    pub fn swap_waiting(&self) -> bool {
        self.back.swaps_waiting()
    }
}

impl<M: HostMem> BoardEngine for KonaEngine<M> {
    type Region = Region;

    fn heads(&self) -> u32 {
        1
    }

    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        out.clear();
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        if head != 0 {
            return Ok(None);
        }
        let (w, h, pixels) = self.compose();
        // The compose is 0xAABBGGRR; the scan-out the shells share is 0x00RRGGBB.
        out.extend(pixels.iter().map(|p| (p & 0xff) << 16 | (p & 0xff00) | (p >> 16 & 0xff)));
        Ok((w != 0 && h != 0).then_some((w, h)))
    }

    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        if head != 0 {
            return None;
        }
        let (clock_hz, frame_clocks, fields, lines, generation) = self.back.head_timing()?;
        // The pipe's retrace is the shell's: its clock starts the phase.
        Some(HeadTiming { frame_clocks, clock_hz, fields, lines, epoch_ns: -1, generation })
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        let (w, h) = self.core.managed;
        (head == 0 && w != 0 && h != 0).then_some((w, h))
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if head != 0 {
            return 0;
        }
        1 + self.activity
    }

    #[cfg(feature = "wgpu")]
    fn lend_frame(&mut self, head: u32) -> Result<Option<sgi_engine::LentFrame>, Fault> {
        if head != 0 {
            return Ok(None);
        }
        let generation = self.frame_generation(0);
        let lent = KonaEngine::lend_frame(self).map_err(Fault)?;
        Ok(lent.map(|f| sgi_engine::LentFrame { width: f.width, height: f.height, stride: f.width, words: sgi_engine::FrameWords::Abgr, generation, buffer: Arc::new(f.buffer) }))
    }

    #[cfg(feature = "wgpu")]
    fn compose_into(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if head != 0 {
            return Ok(None);
        }
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        self.back.compose_into(dst, stride_words).map_err(Fault)
    }

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        self.activity += 1;
        if self.dead() {
            return Write::Fault;
        }
        let Some((words, n)) = Self::words(off, bits, val) else { return Write::Fault };
        let words = &words[..n];
        if self.stream.len() + Self::queues(words) > self.cap {
            return Write::Busy;
        }
        // The credit goes with the store's last word, the one that completes it.
        for (i, &(o, v)) in words.iter().enumerate() {
            self.store(o, v, i + 1 == n && self.source.charged);
        }
        self.notice_fault();
        Write::Done
    }

    /// Each word is a 4-byte store at `port`, and takes a credit when the source is charged.
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize {
        self.activity += 1;
        if self.dead() {
            return 0;
        }
        let take = if classify(port, true) == Access::Now { words.len() } else { words.len().min(self.cap.saturating_sub(self.stream.len())) };
        for &w in &words[..take] {
            self.store(port, w, self.source.charged);
        }
        self.notice_fault();
        take
    }

    fn read(&mut self, off: u32, bits: u32) -> Read {
        if self.dead() {
            return Read::Fault;
        }
        match bits {
            32 if self.read_waits(off) => Read::Busy,
            32 => Read::Value(self.read32(off) as u64),
            64 if self.read_waits(off) || self.read_waits(off.wrapping_add(4)) => Read::Busy,
            64 => {
                let hi = self.read32(off);
                Read::Value((hi as u64) << 32 | self.read32(off.wrapping_add(4)) as u64)
            }
            _ => Read::Fault,
        }
    }

    fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        if self.dead() {
            return false;
        }
        if write {
            Self::words(off, bits, 0).is_some_and(|(w, n)| self.stream.len() + Self::queues(&w[..n]) > self.cap)
        } else {
            match bits {
                32 => self.read_waits(off),
                64 => self.read_waits(off) || self.read_waits(off.wrapping_add(4)),
                _ => false,
            }
        }
    }

    fn run(&mut self, max_ops: usize) -> usize {
        if self.dead() {
            return 0;
        }
        self.run_stream(max_ops)
    }

    fn frame(&mut self, _now_ns: u64) {
        self.activity += 1;
        self.back.frame();
    }

    fn advance(&mut self, _now_ns: u64) -> Pump {
        self.events.out.rearm();
        self.notice_fault();
        Pump { idle: self.idle(), events: self.events.out.pending(), next_deadline: None, irq_levels: self.irq_levels() }
    }

    /// Runs the stream a slice at a time, so a caller with no budget still makes progress.
    fn wait_idle(&mut self, _now_ns: u64, budget_ns: u64) -> Idle {
        let t0 = Instant::now();
        while self.idle() == Idle::Working {
            self.run_stream(SLICE);
            if t0.elapsed() >= Duration::from_nanos(budget_ns) {
                break;
            }
        }
        self.idle()
    }

    /// KONA signals by packets only: no line has a level.
    fn irq_levels(&self) -> u64 {
        0
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        self.events.out.drain(out, max);
    }

    fn observe(&mut self, _region: &Region) -> Result<(), Fault> {
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        self.back.drain();
        Ok(())
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_parked()?;
        self.back.drain();
        Ok(())
    }

    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        let t0 = Instant::now();
        while !self.dead() && !self.stream.is_empty() {
            if self.stalled() {
                return Err(Refused::Midway("the stream waits at a swap for a frame".into()));
            }
            self.run_stream(SLICE);
            if !self.stream.is_empty() && t0.elapsed() >= Duration::from_nanos(budget_ns) {
                return Err(Refused::Midway(format!("{} stores are still queued", self.stream.len())));
            }
        }
        self.back.quiesce();
        self.parked = true;
        match self.fault_now() {
            Some(f) => Err(Refused::Faulted(f)),
            None => Ok(()),
        }
    }

    fn resume(&mut self) {
        self.parked = false;
        self.back.resume();
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_parked()?;
        let mut w = stream::Writer::default();
        self.host.front.lock().visit(&mut w);
        self.core.visit(&mut w);
        self.back.visit_small_open(&mut w).map_err(Refused::Midway)?;
        self.core.visit_open(&mut w);
        self.credits.visit(&mut w);
        self.back.visit_swaps(&mut w);
        let meta = Meta { version: STATE_VERSION, state_len: w.out.len() as u32, bulk: self.back.bulk_layout() };
        let put = |sink: &mut dyn StateSink, name: &str, index: u64, bytes: Vec<u8>| sink.put(Chunk { name: name.into(), index, bytes }).map_err(|e| e.0);
        let sent = (|| {
            put(sink, "meta", 0, postcard::to_allocvec(&meta).map_err(|e| e.to_string())?)?;
            for (i, c) in w.out.chunks(CHUNK_SIZE).enumerate() {
                put(sink, "state", i as u64, c.to_vec())?;
            }
            self.back.save_stream(&mut |name, index, bytes| put(sink, name, index, bytes))
        })();
        sent.map_err(Refused::Faulted)
    }

    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused> {
        self.activity += 1;
        self.need_parked()?;
        let bad = Refused::Mismatch;
        let next = |src: &mut dyn StateSource| src.next().map_err(|e| bad(e.0));
        let first = next(src)?.ok_or_else(|| bad("state is empty".into()))?;
        if first.name != "meta" {
            return Err(bad("state does not start with meta".into()));
        }
        let meta: Meta = postcard::from_bytes(&first.bytes).map_err(|e| bad(format!("meta: {e}")))?;
        if !(1..=STATE_VERSION).contains(&meta.version) {
            return Err(bad(format!("state version {} is not 1 to {STATE_VERSION}", meta.version)));
        }
        let open = meta.version >= 2;
        let swaps = meta.version >= 3;
        let by_source = meta.version >= 4;
        if meta.state_len as usize > 1 << 28 {
            return Err(bad(format!("a state blob of {} bytes", meta.state_len)));
        }
        let mut loader = self.back.bulk_loader(&meta.bulk).map_err(bad)?;
        let mut blob = vec![0u8; meta.state_len as usize];
        while let Some(c) = next(src)? {
            if c.name == "state" {
                let at = (c.index as usize).saturating_mul(CHUNK_SIZE);
                let len = blob.len().saturating_sub(at).min(CHUNK_SIZE);
                if at >= blob.len() || c.bytes.len() != len {
                    return Err(bad(format!("state piece {} does not fit", c.index)));
                }
                blob[at..at + len].copy_from_slice(&c.bytes);
            } else {
                loader.put(&c.name, c.index, &c.bytes).map_err(bad)?;
            }
        }
        let prepared = self.back.finish_loader(loader).map_err(bad)?;
        {
            let cfg = self.core.cfg.clone();
            let mut r = stream::Reader::new(&blob);
            Front::new(&cfg).visit(&mut r);
            let mut core = Core::new(cfg, Flash::fresh());
            core.visit(&mut r);
            if open {
                KonaBackend::check_small_open(&mut r);
                core.visit_open(&mut r);
                if by_source {
                    Credits::default().visit(&mut r);
                } else {
                    Credits::default().visit_scalar(&mut r);
                }
                if swaps {
                    KonaBackend::check_swaps(&mut r);
                }
            } else {
                KonaBackend::check_small(&mut r);
            }
            r.finish().map_err(bad)?;
        }
        let mut r = stream::Reader::new(&blob);
        self.host.front.lock().visit(&mut r);
        self.core.visit(&mut r);
        let mut credits = Credits::default();
        if open {
            self.back.visit_small_open(&mut r).map_err(bad)?;
            self.core.visit_open(&mut r);
            if by_source {
                credits.visit(&mut r);
            } else {
                credits.visit_scalar(&mut r);
            }
            if swaps {
                self.back.visit_swaps(&mut r);
            } else {
                self.back.clear_swaps();
            }
        } else {
            self.back.visit_small(&mut r).map_err(bad)?;
        }
        r.finish().map_err(bad)?;
        self.credits = credits;
        self.back.apply_chunks(prepared).map_err(bad)?;
        self.host.activity.store(false, Relaxed);
        self.events.out.post(Event::Irq);
        self.core.flash.write_through();
        self.report_widget();
        Ok(())
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    /// Power-on state. The flash and the configuration stay.
    fn reset(&mut self) -> Result<(), Refused> {
        self.activity += 1;
        if !self.stream.is_empty() {
            return Err(Refused::NotQuiesced);
        }
        self.core.reset();
        // A new epoch before the back end cancels its tickets.
        self.host.front.lock().advance_epoch();
        self.back.reset();
        self.host.front.lock().reset();
        self.host.activity.store(false, Relaxed);
        self.credits = Credits::default();
        self.events.out.clear();
        *self.fault.lock() = None;
        self.events.out.post(Event::Irq);
        self.report_widget();
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.fault_now()
    }

    fn fail(&self, why: &str) {
        let mut f = self.fault.lock();
        if f.is_none() {
            *f = Some(why.to_string());
            self.events.out.post(Event::DeviceFault(why.to_string()));
        }
    }
}
