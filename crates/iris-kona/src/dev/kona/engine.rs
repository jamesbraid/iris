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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sgi_engine::{BoardEngine, Chunk, Event, Fault, HostMem, Idle, Interrupt, Outbox, Pump, Read, Rect, Refused, Source, StallOn, StateSink, StateSource, Swap, Write};

use super::back::snapshot::Layout;
use super::back::KonaBackend;
use super::defs::*;
use super::flash::Flash;
use super::front::{Front, Host, InterruptPacket};
use super::handle::DisplayHandle;
use super::ident::KonaConfig;
use super::pipe::{Core, LoadSection};
use super::regs::{classify, Access};
use super::stream;
use crate::chunk_store::CHUNK_SIZE;
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BUS_ERR, BUS_OK};
use crate::SharedGpu;

/// The version of the state `save` writes.
pub const STATE_VERSION: u32 = 1;

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

/// What a pipe is built with.
pub struct Options {
    /// The pixel store and the drawing on the GPU, when the shell has one.
    pub gpu: Option<SharedGpu>,
    /// The most stores the command stream holds before `write` answers busy.
    pub queue: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { gpu: None, queue: 1 << 16 }
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

/// Credits owed to Hubs. QEMU's rule, which the model ported: a charged
/// store's credit goes back once the pipe has taken its words, and the
/// credits of a packet still arriving go back together with its last word.
#[derive(Default)]
struct Credits {
    held: u32,
    parked: u32,
    /// The sender of the last store, who a settle with no store of its own pays.
    last: u32,
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
    fault: Mutex<Option<String>>,
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
        Ok(KonaEngine {
            host,
            core: Core::new(cfg, Flash::fresh()),
            back,
            stream: VecDeque::new(),
            cap: opts.queue,
            events,
            credits: Credits::default(),
            pixels,
            source: Source::default(),
            parked: false,
            fault: Mutex::new(None),
            _mem: std::marker::PhantomData,
        })
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
        // The credit register is the FIFO's fill: the words the framers hold and the stores
        // waiting, rounded up to two. It reads full while a display list runs.
        let credit = if self.host.list_running.load(Relaxed) {
            CREDIT_MASK
        } else {
            ((self.core.held_words() + self.stream.len() as u32 + 1) & !1).min(CREDIT_MASK)
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
        let parked_before = self.credits.parked;
        host.front.lock().write(off, value, idle, &mut packet);
        if let Some(p) = packet {
            host.deliver(p);
        }
        if off == LOW_WINDOW_WRITE && value & 1 == 0 && parked_before != 0 {
            let parked = std::mem::take(&mut self.credits.parked);
            self.give(self.source.id, parked);
        }
        if off == INT_CAUSE && self.core.reasons_waiting() {
            self.core.reason_drain(&host);
        }
    }

    /// The Hub's accounting after the pipe took a store's words (`before`: the packets completed
    /// before it). `hub` is the store's source id; 0 is a store with no sender.
    fn settle(&mut self, counted: bool, before: u64, hub: u32) {
        let open = self.core.held_words() != 0;
        let credits = if !open {
            std::mem::take(&mut self.credits.held) + counted as u32
        } else if self.core.packets != before {
            std::mem::replace(&mut self.credits.held, counted as u32)
        } else {
            self.credits.held += counted as u32;
            return;
        };
        self.give(hub, credits);
    }

    /// Return `credits` to `hub`, unless the driver has parked the credit target, when they wait.
    fn give(&mut self, hub: u32, credits: u32) {
        if credits == 0 {
            return;
        }
        if self.host.front.lock().low_window_write & 1 != 0 {
            self.credits.parked += credits;
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
        self.notice_fault();
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

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
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
        if self.back.swaps_waiting() {
            return Err(Refused::Midway("a swap waits for its retrace".into()));
        }
        self.core.at_boundary().map_err(Refused::Midway)?;
        let mut w = stream::Writer::default();
        self.host.front.lock().visit(&mut w);
        self.core.visit(&mut w);
        self.back.visit_small(&mut w).map_err(Refused::Midway)?;
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
        self.need_parked()?;
        let bad = Refused::Mismatch;
        let next = |src: &mut dyn StateSource| src.next().map_err(|e| bad(e.0));
        let first = next(src)?.ok_or_else(|| bad("state is empty".into()))?;
        if first.name != "meta" {
            return Err(bad("state does not start with meta".into()));
        }
        let meta: Meta = postcard::from_bytes(&first.bytes).map_err(|e| bad(format!("meta: {e}")))?;
        if meta.version != STATE_VERSION {
            return Err(bad(format!("state version {} is not {STATE_VERSION}", meta.version)));
        }
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
            Core::new(cfg, Flash::fresh()).visit(&mut r);
            KonaBackend::check_small(&mut r);
            r.finish().map_err(bad)?;
        }
        let mut r = stream::Reader::new(&blob);
        self.host.front.lock().visit(&mut r);
        self.core.visit(&mut r);
        self.back.visit_small(&mut r).map_err(bad)?;
        r.finish().map_err(bad)?;
        self.back.apply_chunks(prepared).map_err(bad)?;
        self.host.activity.store(false, Relaxed);
        self.events.out.post(Event::Irq);
        Ok(())
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    /// Power-on state. The flash and the configuration stay.
    fn reset(&mut self) -> Result<(), Refused> {
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
