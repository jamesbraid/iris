//! One KONA pipe's front end, token layer and drawing behind `sgi_engine::BoardEngine`.
//!
//! The shell calls `write` and `read` for the bus, `advance` at its pump and `frame` at each retrace
//! it chooses. Interrupt packets and credits go to the event queue the shell drains; host memory is
//! the shell's `HostMem`. Iris's front end is mounted unchanged. This file is iris's board routing
//! plus Hub credits.
//!
//! The pipe's stream waits at a swap until the retrace, so the stream stops there and `Idle` says
//! `Stalled(Frame)` until the shell calls `frame`. Drawing runs on the pipe's tile workers, or on the
//! caller's thread when the configuration has none.
//!
//! Threads, locks and calls:
//! - `Options::threaded` runs the command stream on one worker thread. A store to the stream is staged
//!   on the caller's side and handed to the worker by `advance`, a read, or a full stage; the worker
//!   runs the stores in order and posts the credits and packets they owe. Without it (the default)
//!   there is no thread: `run` and `wait_idle` run the stream on the caller's thread, and its host DMA
//!   goes through `HostMem::bus_*`; the worker's goes through the lock-free `ram_*`.
//! - A register the stream does not own (`classify`) is served on the caller's thread from the
//!   register file and atomics, whatever the worker is doing. Completion reasons a cause-register
//!   access finds waiting are posted by that access when the pipe is free, else by the worker
//!   (`Shared::poke`). Any other read answers `Busy` until the stream has run, and takes the pipe
//!   with `try_lock`.
//! - Lock order, everywhere: `Shared::inner` (the pipe's state and its back end), then
//!   `Shared::credits`, then the register file (`Host::front`). The queue, the widget callback, the
//!   fault text and the outbox are leaves, never held while taking another. The worker holds `inner`
//!   for a run of stores and `bump`s it between them, so a caller that needs it waits for one store.
//! - A store to the map RAM with nothing ahead of it runs on the caller's thread (`store`), and the display option
//!   board's bus callbacks (`set_display_bus`) run on the worker.
//! - A panic in the worker or in `run` is a device fault: the engine stops, the stores still queued
//!   are dropped, and every call answers `Fault` until `reset`.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use sgi_engine::inject::{hit, Point};
use sgi_engine::{BoardEngine, Chunk, Event, Fault, HeadTiming, HostMem, Idle, Interrupt, Outbox, Pump, Read, Rect, Refused, Source, StallOn, StateSink, StateSource, Swap, Write};

use super::back::snapshot::Layout;
use super::back::display::option::{low_bits, DisplayBusSlave};
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
/// the next frame; 4 keeps the credits held and parked by source; 5 whether the display option board's
/// channel exists; 6 the registers the adaptor reset writes; 7 the display-bus words written. A load takes all seven.
pub const STATE_VERSION: u32 = 7;

/// Stores `quiesce` and `wait_idle` run between looks at the clock.
const SLICE: usize = 1 << 12;
/// Stores a caller stages before it hands them to the queue.
const STAGE_MAX: usize = 256;
/// Stores a runner takes from the queue at a time.
const BATCH_MAX: usize = 64;
/// A run that does not drain publishes its view and generation every this many batches.
const PUBLISH_BATCHES: usize = 16;

/// Host memory as the pipe's DMA reaches it: physical addresses before the fabric adds the widget.
/// Inline, the DMA runs on the caller's thread inside a call and uses the `bus_*` path; the worker's
/// takes the lock-free `ram_*` path, which reaches RAM only.
struct Mem<M: HostMem> {
    mem: M,
    bus: bool,
}

impl<M: HostMem> BusDevice for Mem<M> {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> u32 {
        let r = if self.bus { self.mem.bus_read(addr, buf, Swap::None) } else { self.mem.ram_read(addr, buf, Swap::None) };
        if r.is_ok() { BUS_OK } else { BUS_ERR }
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> u32 {
        let r = if self.bus { self.mem.bus_write(addr, data, Swap::None) } else { self.mem.ram_write(addr, data, Swap::None) };
        if r.is_ok() { BUS_OK } else { BUS_ERR }
    }
}

/// The shell's event queue, and where the pipe's interrupt packets go. `Outbox` keeps one packet for
/// each line, so a guest that raises a vector faster than the shell drains adds nothing.
struct Events {
    out: Outbox,
    /// The widget the host's packets go to (the DMA widget the driver programmed).
    host_widget: Arc<AtomicU8>,
    /// The shell time the pipe last had, and the swaps consumed since the build or reset (`Event::Swap`).
    clock_ns: AtomicU64,
    swaps: AtomicU64,
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

    fn swap_consumed(&self) {
        let count = self.swaps.fetch_add(1, Relaxed) + 1;
        self.out.post(Event::Swap { count, time_ns: self.clock_ns.load(Relaxed) });
    }

    fn swap_ready(&self, drive: bool) {
        self.out.post(Event::SwapReady { drive });
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
    /// Told the widget the pipe's DMA goes to, now and at each change the guest makes. Called from
    /// whichever thread changed it, the worker's included.
    pub dma_widget: Option<Box<dyn Fn(u8) + Send>>,
    /// Run the command stream on a worker thread (`IrisEngineConfig::mode` 1); false runs it inside
    /// `run` and `wait_idle`.
    pub threaded: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { gpu: None, queue: 1 << 16, flash: None, dma_widget: None, threaded: false }
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
    /// What the framers hold and have completed as of the last store the stream ran, kept here so a
    /// store the caller settles and one the worker settles see one state. Not saved.
    open: u32,
    packets: u64,
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

/// The managed rows `first..end` a head whose window is `h` rows from row `y` shows, as its own rows.
pub fn head_rows((first, end): (usize, usize), y: u32, h: u32) -> Option<(u32, u32)> {
    let (lo, hi) = (first.max(y as usize), end.min((y + h) as usize));
    (lo < hi).then(|| ((lo - y as usize) as u32, (hi - y as usize) as u32))
}

/// A head's window and blanking as they were when its `ScanSource` was made.
#[derive(Clone, Copy)]
struct HeadView {
    window: (u32, u32, u32, u32),
    blanked: bool,
}

/// One head as the shell may ask after it without waiting for the stream.
#[derive(Clone, Copy)]
struct HeadInfo {
    window: Option<(u32, u32, u32, u32)>,
    /// The window lies in the managed area the pixels are held for.
    fits: bool,
    blanked: bool,
    option: bool,
    /// The head shows the whole managed area and is not blanked.
    whole: bool,
    timing: Option<HeadTiming>,
}

/// What `heads`, `head_timing`, `frame_size` and the channel windows answer, as of the end of the last run of stores,
/// frame, load or reset: the stream changes it only from inside those, so it is kept apart from `Shared::inner`, which
/// the worker holds for a store.
#[derive(Clone, Default)]
struct View {
    heads: Vec<HeadInfo>,
    /// Channel `i`'s window (`KonaBackend::channel_window`).
    windows: Vec<Option<(u32, u32, u32, u32)>>,
    /// The channel a request's word names (`KonaBackend::channel_index`), for the words a request can use.
    words: Vec<(u32, usize)>,
    cursor: (i32, i32, bool),
}

impl View {
    fn head(&self, head: u32) -> Option<&HeadInfo> {
        self.heads.get(head as usize)
    }

    fn index_of(&self, channel: u32) -> Option<usize> {
        self.words.iter().find(|w| w.0 == channel).map(|w| w.1)
    }

    fn window_of(&self, channel: u32) -> Option<(u32, u32, u32, u32)> {
        self.windows.get(self.index_of(channel)?).copied().flatten()
    }
}

/// The head's size, and whether `rows` composed (`Short`: the buffer was too small).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scan {
    Done(u32, u32),
    Short(u32, u32),
}

/// The heads' pictures without the engine's handle (`sgi_kona.h`): any thread may call `rows`. Lock order: `band`,
/// then the colour map and the tables, one at a time; nothing takes `band` while it holds either.
pub struct ScanSource {
    pixels: super::back::display::Pixels,
    band: Arc<Mutex<Vec<u32>>>,
    heads: Vec<Option<HeadView>>,
    sh: Arc<Shared>,
}

impl ScanSource {
    /// Rows `first..end` (clamped) of head `head`'s picture into `out`, as `0x00RRGGBB` at the head's width a row
    /// from its first; the other words are left alone. A head with no picture is `Done(0, 0)`.
    pub fn rows(&self, head: u32, first: usize, end: usize, out: &mut [u32]) -> Result<Scan, Fault> {
        if let Some(f) = self.sh.fault.lock().clone() {
            return Err(Fault(f));
        }
        let Some(Some(view)) = self.heads.get(head as usize) else { return Ok(Scan::Done(0, 0)) };
        let (_, _, w, h) = view.window;
        if out.len() < w as usize * h as usize {
            return Ok(Scan::Short(w, h));
        }
        self.pixels.window_rows(view.window, first..end, view.blanked, out, &mut self.band.lock());
        Ok(Scan::Done(w, h))
    }

    /// Stop the engine with `why`: a panic in a call.
    pub fn fail(&self, why: &str) {
        self.sh.fail(why);
    }
}

fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

struct Queue {
    stream: VecDeque<Stored>,
    stop: bool,
}

/// The pipe's own state, which the stream changes: the framers and pages, and the back end that draws.
struct Inner {
    core: Core,
    back: KonaBackend,
    pixels: super::back::display::Pixels,
    /// Scratch frame rows, kept between scan-outs.
    band: Arc<Mutex<Vec<u32>>>,
}

/// What the caller's calls and the worker share.
struct Shared {
    inner: Mutex<Inner>,
    q: Mutex<Queue>,
    /// The worker waits on `work_cv` for stores, a poke or a frame; a caller on `idle_cv` for the stream to run.
    work_cv: Condvar,
    idle_cv: Condvar,
    host: Arc<Host>,
    events: Arc<Events>,
    credits: Mutex<Credits>,
    fault: Mutex<Option<String>>,
    /// `fault` is set: read by the calls that answer `Fault` without taking a lock.
    dead: AtomicBool,
    /// The widget the shell was last told, and who tells it.
    widget: Mutex<(Option<u8>, Option<Box<dyn Fn(u8) + Send>>)>,
    /// Stores handed on from the stage, and those the stream has run or, once dead, dropped.
    sent: AtomicU64,
    done: AtomicU64,
    /// Moves with every run of stores, and every frame, reset and load (`frame_generation`).
    ran: AtomicU64,
    /// A swap holds the stream until the next frame.
    swap_wait: AtomicBool,
    /// A cause-register access found completion reasons waiting behind an unread one: the stream's runner posts them.
    poke: AtomicBool,
    /// The worker thread runs, so reasons are posted by it.
    worker_live: AtomicBool,
    /// Wall time the worker has spent running stores, and the tile workers' as the last look at them found it.
    busy_ns: AtomicU64,
    tile_ns: AtomicU64,
    view: Mutex<View>,
    /// The swap-ready line is held low by another pipe; `frame` hands it to the back end.
    swap_held: AtomicBool,
}

impl Shared {
    fn dead(&self) -> bool {
        self.dead.load(Acquire)
    }

    /// Stop the engine with `why`; the first fault stays.
    fn fail(&self, why: &str) {
        let mut f = self.fault.lock();
        if f.is_none() {
            *f = Some(why.to_string());
            self.dead.store(true, Release);
            drop(f);
            self.events.out.post(Event::DeviceFault(why.to_string()));
            // A caller waiting for the stream stops waiting, and the worker drops what is queued.
            let _q = self.q.lock();
            self.work_cv.notify_all();
            self.idle_cv.notify_all();
        }
    }

    /// Latch a fault the pipe found itself, and tell the shell once: a failed stream or a panicked tile worker.
    fn notice(&self, i: &Inner) {
        if self.dead() {
            return;
        }
        if let Some(f) = i.core.failed.clone() {
            self.fail(&f);
        } else if i.back.is_poisoned() {
            self.fail("a tile worker panicked; the pipe is dead");
        }
    }

    /// Tell the shell the widget the pipe's DMA goes to, if it is not the one it was last told.
    fn report_widget(&self) {
        let now = self.events.host_widget.load(Relaxed);
        let mut w = self.widget.lock();
        if w.0 != Some(now) {
            w.0 = Some(now);
            if let Some(f) = &w.1 {
                f(now);
            }
        }
    }

    /// Return `credits` to `hub`, unless the driver has parked the credit target, when they wait for it, by source.
    fn give(&self, c: &mut Credits, hub: u32, credits: u32) {
        if credits == 0 {
            return;
        }
        if self.host.front.lock().low_window_write & 1 != 0 {
            *c.parked.entry(hub).or_default() += credits;
            return;
        }
        self.events.out.post(Event::Credits { source: hub, retired: credits });
    }

    /// The Hub's accounting after the pipe took a store's words (`before`: the packets completed
    /// before it). `hub` is the store's source id; 0 is a store with no sender.
    fn settle(&self, c: &mut Credits, counted: bool, before: u64, hub: u32) {
        let mut owed = if c.open == 0 {
            // Nothing holds credits back: everything held goes, each to its source, and this store's own with it.
            let mut owed = std::mem::take(&mut c.held);
            *owed.entry(hub).or_default() += counted as u32;
            owed
        } else if c.packets != before {
            // This store completed a packet: the held credits go to the sources that earned them, and this
            // store's own starts the next packet's hold.
            std::mem::replace(&mut c.held, counted.then_some((hub, 1)).into_iter().collect())
        } else {
            if counted {
                *c.held.entry(hub).or_default() += 1;
            }
            return;
        };
        owed.retain(|_, n| *n != 0);
        for (hub, n) in owed {
            self.give(c, hub, n);
        }
    }

    /// A store to a register the stream does not own: the register file takes it now.
    fn write_now(&self, off: u32, value: u32, drained: bool) {
        if off == ACTIVITY {
            if value & ACTIVITY_BUS != 0 {
                self.host.activity.store(false, Relaxed);
            }
            return;
        }
        let mut packet: Option<InterruptPacket> = None;
        let idle = off == INT_ENABLE && drained;
        self.host.front.lock().write(off, value, idle, &mut packet);
        if let Some(p) = packet {
            self.host.deliver(p);
        }
        if off == LOW_WINDOW_WRITE && value & 1 == 0 {
            let mut c = self.credits.lock();
            for (hub, n) in std::mem::take(&mut c.parked) {
                self.give(&mut c, hub, n);
            }
        }
        if off == INT_CAUSE {
            self.drain_reasons();
        }
        self.report_widget();
    }

    /// A read of a register the stream does not own, with `pending` stores not yet run.
    fn read_now(&self, off: u32, pending: usize) -> u32 {
        match off {
            INT_CAUSE => self.drain_reasons(),
            ACTIVITY => return if self.host.activity.load(Relaxed) || pending != 0 { ACTIVITY_BUS } else { 0 },
            _ => {}
        }
        // The FIFO's fill is a word count in bits 10:1. It reads full while a display list runs.
        let credit = if self.host.list_running.load(Relaxed) { CREDIT_MASK } else { ((self.host.held_words.load(Relaxed) + pending as u32) << 1).min(CREDIT_MASK) };
        self.host.front.lock().read(off, credit).unwrap_or(0)
    }

    /// Completion reasons waiting behind an unread one are posted here when the pipe is free, else by the worker.
    fn drain_reasons(&self) {
        if !self.host.reasons_waiting.load(Acquire) {
            return;
        }
        // A worker between runs leaves the pipe free: post here, as an inline engine does. A busy one posts them.
        if let Some(mut i) = self.inner.try_lock() {
            self.post_reasons(&mut i);
        } else if self.worker_live.load(Acquire) {
            self.poke.store(true, Release);
            let _q = self.q.lock();
            self.work_cv.notify_all();
        }
    }

    fn post_reasons(&self, i: &mut Inner) {
        i.core.reason_drain(&self.host);
        self.host.reasons_waiting.store(i.core.reasons_waiting(), Release);
    }

    fn publish_view(&self, i: &Inner) {
        *self.view.lock() = i.view();
    }

    /// What the polled registers read of the pipe, and whether a swap holds the stream.
    fn publish(&self, c: &mut Credits, i: &Inner) {
        c.open = i.core.held_words();
        c.packets = i.core.packets;
        self.host.held_words.store(c.open, Release);
        self.host.reasons_waiting.store(i.core.reasons_waiting(), Release);
        self.swap_wait.store(i.back.swaps_waiting(), Release);
    }

    /// One store of the stream, and the credit it earned.
    fn exec(&self, i: &mut Inner, s: Stored) {
        hit(Point::Exec);
        let before = i.core.packets;
        match classify(s.off, true) {
            Access::Waits => i.core.write_wait(&self.host, &mut i.back, s.off, s.val),
            _ => i.core.stream_store(&self.host, &mut i.back, s.off, s.val),
        }
        let mut c = self.credits.lock();
        self.publish(&mut c, i);
        if s.src.charged || s.src.id != 0 {
            c.last = s.src.id;
        }
        self.settle(&mut c, s.counted, before, s.src.id);
        drop(c);
        self.notice(i);
    }

    /// Drop the stores still queued, for an engine that has faulted.
    fn abandon(&self) {
        let mut q = self.q.lock();
        q.stream.clear();
        self.done.store(self.sent.load(Acquire), Release);
        self.idle_cv.notify_all();
    }

    /// Run the stream, in order, until it is empty, waits at a swap, or `max` stores have run. The worker's
    /// loop, and the caller's with none. A panic is a device fault.
    fn run(&self, max: usize) -> usize {
        let mut ran = 0;
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| self.run_stores(max, &mut ran))) {
            self.fail(&format!("the engine panicked: {}", panic_text(&*p)));
        }
        if self.dead() {
            self.abandon();
        }
        {
            let _q = self.q.lock();
            self.idle_cv.notify_all();
        }
        ran
    }

    fn run_stores(&self, max: usize, ran: &mut usize) {
        let mut inner = self.inner.lock();
        let mut batch: VecDeque<Stored> = VecDeque::with_capacity(BATCH_MAX);
        let mut since = 0usize;
        while *ran < max && !self.dead() && !inner.back.swaps_waiting() {
            {
                let mut q = self.q.lock();
                let n = q.stream.len().min(BATCH_MAX).min(max - *ran);
                batch.extend(q.stream.drain(..n));
            }
            if batch.is_empty() {
                break;
            }
            while let Some(s) = batch.pop_front() {
                if inner.back.swaps_waiting() || self.dead() {
                    // The rest waits for the frame, or goes with the fault.
                    let mut q = self.q.lock();
                    batch.push_front(s);
                    while let Some(x) = batch.pop_back() {
                        q.stream.push_front(x);
                    }
                    break;
                }
                self.exec(&mut inner, s);
                *ran += 1;
                self.done.fetch_add(1, Release);
                MutexGuard::bump(&mut inner);
            }
            // A run that never drains still shows the shell what it has changed.
            since += 1;
            if since >= PUBLISH_BATCHES {
                self.publish_view(&inner);
                self.ran.fetch_add(1, Relaxed);
                since = 0;
            }
        }
        // Reasons a cause-register access found waiting are posted here too.
        self.poke.store(false, Release);
        self.post_reasons(&mut inner);
        if *ran != 0 && self.q.lock().stream.is_empty() {
            // Credits the framers held while the stores were arriving go back once nothing holds them.
            let mut c = self.credits.lock();
            let (before, last) = (c.packets, c.last);
            self.settle(&mut c, false, before, last);
            drop(c);
            self.host.with_front(|f| ((), f.lowater()));
        }
        self.ran.fetch_add((*ran != 0) as u64, Relaxed);
        if *ran != 0 {
            self.publish_view(&inner);
        }
        self.tile_ns.store(inner.back.busy_ns(), Relaxed);
        self.notice(&inner);
        self.report_widget();
    }
}

/// The worker: runs the stream until it is told to stop with nothing left to run.
fn worker(sh: Arc<Shared>) {
    loop {
        {
            let mut q = sh.q.lock();
            while !q.stop && !sh.poke.load(Acquire) && (q.stream.is_empty() || sh.swap_wait.load(Acquire) || sh.dead()) {
                sh.work_cv.wait(&mut q);
            }
            // Stores held behind a swap or left by a fault are not run on the way out.
            if q.stop && !sh.poke.load(Acquire) && (q.stream.is_empty() || sh.swap_wait.load(Acquire) || sh.dead()) {
                return;
            }
        }
        let began = Instant::now();
        sh.run(usize::MAX);
        sh.busy_ns.fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
        // A shell access that found the engine busy waits for this.
        sh.events.out.wake();
    }
}

impl Inner {
    fn view(&self) -> View {
        let heads = (0..self.back.running_channels().len().max(1) as u32).map(|h| self.head_info(h)).collect();
        let managed = self.core.managed;
        let windows = (0..super::back::display::channels::CHANNELS).map(|c| self.back.channel_window(c, managed)).collect();
        let words = (0..=super::back::display::channels::CHANNELS as u32 + 1).chain([super::back::display::channels::GVO_ID]).filter_map(|w| Some((w, self.back.channel_index(w)?))).collect();
        View { heads, windows, words, cursor: self.back.dac_cursor() }
    }

    fn head_info(&self, head: u32) -> HeadInfo {
        let window = self.head_window(head);
        let timing = self.back.running_channels().get(head as usize).and_then(|&channel| {
            let (clock_hz, frame_clocks, fields, lines, generation) = self.back.channel_timing(channel)?;
            // The pipe's retrace is the shell's: its clock starts the phase.
            Some(HeadTiming { frame_clocks, clock_hz, fields, lines, epoch_ns: -1, generation })
        });
        HeadInfo { window, fits: window.is_some_and(|w| self.window_fits(w)), blanked: self.head_blanked(head), option: self.head_is_option(head), whole: self.head_is_whole(head), timing }
    }

    /// Head `head`'s window of the managed area, (x, y from the top, width, height): its channel's input
    /// rectangle (the whole area for a channel that has not been resized), or the whole area while no channel
    /// runs; None for no such head.
    fn head_window(&self, head: u32) -> Option<(u32, u32, u32, u32)> {
        let (w, h) = self.core.managed;
        if w == 0 || h == 0 {
            return None;
        }
        let running = self.back.running_channels();
        match running.get(head as usize) {
            Some(&c) => Some(self.back.channel_window(c, self.core.managed).unwrap_or((0, 0, w, h))),
            None => (head == 0 && running.is_empty()).then_some((0, 0, w, h)),
        }
    }

    /// Head `head` shows the whole managed area, the only frame a GPU buffer can be lent for; a blanked head shows
    /// none of it.
    fn head_is_whole(&self, head: u32) -> bool {
        self.head_window(head) == Some((0, 0, self.core.managed.0, self.core.managed.1)) && !self.head_blanked(head)
    }

    /// Head `head` is the option board's channel.
    fn head_is_option(&self, head: u32) -> bool {
        let running = self.back.running_channels();
        running.get(head as usize).is_some_and(|&c| Some(c) == self.back.option_channel())
    }

    /// Head `head`'s channel is blanked (`Back::channel_blanked`): its output is black whatever the frame holds.
    fn head_blanked(&self, head: u32) -> bool {
        self.back.running_channels().get(head as usize).is_some_and(|&c| self.back.channel_blanked(c))
    }

    /// Whether `window` lies in the managed area the pixels are held for.
    fn window_fits(&self, (x, y, w, h): (u32, u32, u32, u32)) -> bool {
        let (fw, fh) = self.pixels.size();
        x + w <= fw && y + h <= fh
    }

    /// The frame the display would show now: `width * height` words from the top, `0xAABBGGRR`.
    fn compose(&mut self) -> (u32, u32, Vec<u32>) {
        self.back.drain();
        let (w, h) = self.core.managed;
        (w, h, self.pixels.frame())
    }

    /// As `compose`, the managed area with ten of the gamma RAM's twelve bits a channel as `x2b10g10r10` (red low),
    /// without the DAC cursor: the frame a head whose DAC takes ten bits shows.
    fn compose_wide(&mut self) -> (u32, u32, Vec<u32>) {
        self.back.drain();
        let (w, h) = self.core.managed;
        (w, h, self.pixels.frame_wide())
    }

    /// The option board's head with its ten bits: the window of the wide frame (`compose_wide`), black while the
    /// channel is blanked, its two low bits formed as the board's control register says (0xc006 bits 10-9: 0 zero,
    /// 2 the gamma's own bits, 3 a copy of the top two bits; 1 is unnamed, shown as 2).
    fn scanout_gvo(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        out.clear();
        let Some((x, y, w, h)) = self.head_window(head) else { return Ok(None) };
        let (fw, _, frame) = self.compose_wide();
        let blanked = self.head_blanked(head);
        let mode = self.back.option_control().map_or(2, |c| c >> 9 & 3);
        let channel = |p: u32| low_bits(p & 0x3ff, mode) | low_bits(p >> 10 & 0x3ff, mode) << 10 | low_bits(p >> 20 & 0x3ff, mode) << 20;
        for row in y..y + h {
            let at = (row * fw + x) as usize;
            match frame.get(at..at + w as usize) {
                Some(px) if !blanked => out.extend(px.iter().map(|&p| channel(p))),
                Some(_) => out.extend(std::iter::repeat_n(0, w as usize)),
                None => {
                    out.clear();
                    return Ok(None);
                }
            }
        }
        Ok((w != 0 && h != 0).then_some((w, h)))
    }

    /// The pixels of `window` of the composed frame, `0x00RRGGBB` in rows from the top, with its size.
    fn window_pixels(&mut self, window: (u32, u32, u32, u32)) -> (u32, u32, Vec<u32>) {
        if !self.window_fits(window) {
            return (0, 0, Vec::new());
        }
        self.back.drain();
        let (_, _, w, h) = window;
        let mut out = vec![0; w as usize * h as usize];
        self.pixels.window_rows(window, 0..h as usize, false, &mut out, &mut self.band.lock());
        (w, h, out)
    }
}

pub struct KonaEngine<M: HostMem> {
    sh: Arc<Shared>,
    cap: usize,
    /// Stores taken but not yet handed to the queue, and every store taken since the build or reset (the stream's
    /// are `Shared::done`).
    stage: Vec<Stored>,
    submitted: u64,
    worker: Option<JoinHandle<()>>,
    threaded: bool,
    pixels: super::back::display::Pixels,
    /// Scratch frame rows, shared with `Inner` and the scan-out sources.
    band: Arc<Mutex<Vec<u32>>>,
    /// The display generator's channels, fixed by the configuration.
    channels: u32,
    /// Per head, the managed rows that changed since the shell last asked, as a hull.
    damage: Vec<Option<(usize, usize)>>,
    source: Source,
    parked: bool,
    /// Moves with every store, push, frame, load and reset (`frame_generation`); the worker's runs are in `Shared::ran`.
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
        let events = Arc::new(Events { out: Outbox::new(notify), host_widget: front.dma_widget_handle(), clock_ns: AtomicU64::new(0), swaps: AtomicU64::new(0) });
        let host = Arc::new(Host::new(front, events.clone(), Arc::new(Mem { mem, bus: !opts.threaded })));
        back.attach_manual(DisplayHandle::new(&host));
        back.latch_swaps(true);
        let flash = match opts.flash {
            Some(d) => Flash::with_sink(&d.image, d.sink)?,
            None => Flash::fresh(),
        };
        let band = Arc::new(Mutex::new(Vec::new()));
        let sh = Arc::new(Shared {
            inner: Mutex::new(Inner { core: Core::new(cfg, flash), back, pixels: pixels.clone(), band: band.clone() }),
            q: Mutex::new(Queue { stream: VecDeque::new(), stop: false }),
            work_cv: Condvar::new(),
            idle_cv: Condvar::new(),
            host,
            events,
            credits: Mutex::new(Credits::default()),
            fault: Mutex::new(None),
            dead: AtomicBool::new(false),
            widget: Mutex::new((None, opts.dma_widget)),
            sent: AtomicU64::new(0),
            done: AtomicU64::new(0),
            ran: AtomicU64::new(0),
            swap_wait: AtomicBool::new(false),
            poke: AtomicBool::new(false),
            worker_live: AtomicBool::new(false),
            busy_ns: AtomicU64::new(0),
            tile_ns: AtomicU64::new(0),
            view: Mutex::new(View::default()),
            swap_held: AtomicBool::new(false),
        });
        sh.publish_view(&sh.inner.lock());
        let mut engine = KonaEngine {
            sh,
            cap: opts.queue,
            stage: Vec::new(),
            submitted: 0,
            worker: None,
            threaded: opts.threaded,
            pixels,
            band,
            channels: cfg.display_channels(),
            damage: vec![None; super::back::display::channels::CHANNELS],
            source: Source::default(),
            parked: false,
            activity: 0,
            _mem: std::marker::PhantomData,
        };
        engine.sh.report_widget();
        engine.spawn();
        Ok(engine)
    }

    fn spawn(&mut self) {
        if self.threaded && self.worker.is_none() {
            self.sh.q.lock().stop = false;
            self.sh.worker_live.store(true, Release);
            let sh = self.sh.clone();
            self.worker = Some(std::thread::Builder::new().name("KONA-engine".into()).spawn(move || worker(sh)).expect("spawn the engine worker"));
        }
    }

    fn stop_worker(&mut self) {
        if let Some(w) = self.worker.take() {
            self.sh.q.lock().stop = true;
            {
                let _q = self.sh.q.lock();
                self.sh.work_cv.notify_all();
            }
            let _ = w.join();
            self.sh.worker_live.store(false, Release);
        }
    }

    /// What a GPU unit's device-loss callback calls once this engine exists (`GpuLoss::attach`): post
    /// `GpuLost`, then fault the engine, which tells the shell.
    pub fn gpu_lost_handler(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let sh = self.sh.clone();
        move |why| {
            sh.events.out.post(Event::GpuLost(why.to_string()));
            sh.fail(why);
        }
    }

    /// The device fault, latched or not yet noticed: what `fail` was told, a failed command stream or
    /// a panicked tile worker.
    fn fault_now(&self) -> Option<String> {
        if !self.sh.dead() {
            if let Some(i) = self.sh.inner.try_lock() {
                self.sh.notice(&i);
            }
        }
        self.sh.fault.lock().clone()
    }

    fn dead(&self) -> bool {
        self.sh.dead()
    }

    /// Latch a fault the pipe found itself, and tell the shell once.
    fn notice_fault(&self) {
        if !self.dead() {
            if let Some(i) = self.sh.inner.try_lock() {
                self.sh.notice(&i);
            }
        }
    }

    /// Stores taken and not yet run, staged or queued.
    fn pending(&self) -> usize {
        self.submitted.saturating_sub(self.sh.done.load(Acquire)) as usize
    }

    /// The stream holds at a swap until the retrace.
    fn stalled(&self) -> bool {
        self.pending() != 0 && self.sh.swap_wait.load(Acquire)
    }

    fn idle(&self) -> Idle {
        if self.dead() {
            Idle::Faulted
        } else if self.pending() == 0 {
            Idle::Yes
        } else if self.stalled() {
            Idle::Stalled(StallOn::Frame)
        } else {
            Idle::Working
        }
    }

    /// Hand the stage to the queue, and the worker its work.
    fn flush(&mut self) {
        if self.stage.is_empty() {
            return;
        }
        self.sh.sent.fetch_add(self.stage.len() as u64, Release);
        self.sh.q.lock().stream.extend(self.stage.drain(..));
        if self.worker.is_some() {
            self.sh.work_cv.notify_one();
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
        classify(off, false) != Access::Now && self.pending() != 0
    }

    /// One word of a store: a register the stream does not own takes it now, the rest queue.
    fn store(&mut self, off: u32, val: u32, counted: bool) {
        if classify(off, true) == Access::Now {
            let idle = self.pending() == 0;
            self.sh.write_now(off, val, idle);
            // The worker may have drained between that look and the enable: it found the cause disabled, and
            // nothing else would raise it. A second raise is one packet more on a line that merges.
            if off == INT_ENABLE && !idle && self.pending() == 0 {
                self.sh.host.with_front(|f| ((), f.lowater()));
            }
            if counted {
                let mut c = self.sh.credits.lock();
                let before = c.packets;
                self.sh.settle(&mut c, true, before, self.source.id);
            }
        } else {
            if self.worker.is_some() && self.pending() == 0 && (MAPRAM_BASE..MAPRAM_BASE + 4 * MAPRAM_ENTRIES as u32).contains(&off) {
                // The driver tests the map RAM a word at a time, a store and a load each: with nothing ahead of
                // the store, run it here so the load does not wait for a trip through the worker.
                if let Some(mut i) = self.sh.inner.try_lock() {
                    self.submitted += 1;
                    self.sh.sent.fetch_add(1, Release);
                    self.sh.exec(&mut i, Stored { off, val, src: self.source, counted });
                    self.sh.done.fetch_add(1, Release);
                    return;
                }
            }
            if self.pending() == 0 {
                self.sh.events.out.wake();
            }
            self.stage.push(Stored { off, val, src: self.source, counted });
            self.submitted += 1;
            if self.stage.len() >= STAGE_MAX {
                self.flush();
            }
        }
    }

    fn need_parked(&self) -> Result<(), Refused> {
        if let Some(f) = self.fault_now() {
            return Err(Refused::Faulted(f));
        }
        if !self.parked || self.pending() != 0 {
            return Err(Refused::NotQuiesced);
        }
        Ok(())
    }

    /// Wait for the worker to run what it has, or to stop at a swap, for `budget` of host time at most; with
    /// no worker, run the stream here. True when nothing is pending.
    fn settle(&mut self, budget: Option<Duration>) -> bool {
        self.flush();
        if self.worker.is_none() {
            let t0 = Instant::now();
            while self.idle() == Idle::Working {
                self.sh.run(SLICE);
                if budget.is_some_and(|b| t0.elapsed() >= b) {
                    break;
                }
            }
            return self.pending() == 0;
        }
        let deadline = budget.map(|b| Instant::now() + b);
        let mut q = self.sh.q.lock();
        while self.pending() != 0 && !self.stalled() && !self.dead() {
            match deadline {
                Some(d) => {
                    if self.sh.idle_cv.wait_until(&mut q, d).timed_out() {
                        break;
                    }
                }
                None => self.sh.idle_cv.wait(&mut q),
            }
        }
        self.pending() == 0
    }

    /// The pixels a CPU reader asks for follow the stores the worker has: wait for them, unless a swap holds the
    /// stream. Inline, the shell runs the stream and this does nothing.
    fn catch_up(&mut self) {
        if self.worker.is_some() {
            self.settle(None);
        }
    }

    // ----- what a shell reads off the board --------------------------------

    /// Word `w` of the pixel at (x, y) as the store holds it, after the draws behind it have run.
    pub fn pixel(&mut self, x: u32, y: u32, w: usize) -> u32 {
        self.catch_up();
        self.sh.inner.lock().back.drain();
        self.pixels.word(x, y, w)
    }

    /// The frame the display would show now: `width * height` words from the top, `0xAABBGGRR`.
    pub fn compose(&mut self) -> (u32, u32, Vec<u32>) {
        self.catch_up();
        self.sh.inner.lock().compose()
    }

    /// As `compose`, the managed area with ten of the gamma RAM's twelve bits a channel as `x2b10g10r10` (red low),
    /// without the DAC cursor: the frame a head whose DAC takes ten bits shows.
    pub fn compose_wide(&mut self) -> (u32, u32, Vec<u32>) {
        self.catch_up();
        self.sh.inner.lock().compose_wide()
    }

    /// The DAC cursor of channel 0 as the display bus's registers hold it, without waiting for queued stores: the
    /// origin of its 64 by 64 image in screen pixels from the top left (signed; the hot spot is the image's own),
    /// and whether it is enabled in X11 mode. For a shell that publishes the pointer.
    pub fn dac_cursor(&self) -> (i32, i32, bool) {
        self.sh.view.lock().cursor
    }

    /// What the GPU store has done: records, batches, copies, readbacks, copies refused for a word too wide, and the ops that
    /// ran on the CPU for want of a GPU kernel. None without a GPU store or once the engine has faulted. Drains the
    /// tile pool first, so the counts include everything submitted.
    #[cfg(feature = "wgpu")]
    pub fn gpu_counters(&mut self) -> Option<[u64; 6]> {
        if self.fault_now().is_some() {
            return None;
        }
        self.sh.inner.lock().back.gpu_counters()
    }

    /// A video-output-chip interrupt for the shell to raise (an external sync reference detected or lost: inferred;
    /// kona `devices/dg/external-status.md`): `HH_INTR_STATUS` bit 9 rises, and the interrupt packet goes out when the
    /// driver has enabled the bit in the mask; write-one-to-clear like the others. The pipe has no external sync of
    /// its own, so nothing else raises it.
    pub fn raise_voc_interrupt(&mut self) {
        self.activity += 1;
        self.sh.host.with_front(|f| ((), f.voc_interrupt()));
    }

    /// The shared swap-ready line is held low by another pipe (the shell's wired-AND of every pipe's
    /// `Event::SwapReady` drive, its own included): the swaps wait at the frame boundary until it is released. The
    /// polarity and timing of the real line are unknown, so this is an approximation.
    pub fn set_swap_ready_held(&mut self, held: bool) {
        self.sh.swap_held.store(held, Release);
    }

    /// The channels the display generator has (8 on a DG5-8, 2 on a DG5-2), as the configuration chose its board;
    /// the option board's channel is one more once it is probed.
    pub fn display_channels(&self) -> u32 {
        self.channels
    }

    /// The shell's device for the display option board (a GVO), or none: it answers the display-bus accesses
    /// the firmware makes for the board (`display::option`), and the board's channel exists once the firmware's
    /// probe of it succeeds. The configuration must allow it (`KonaConfig::with_option_slave`). It stays through
    /// a reset and a load. Its calls come from the thread that runs the store that makes them: the worker's.
    pub fn set_display_bus(&mut self, slave: Option<Box<dyn DisplayBusSlave>>) {
        self.sh.inner.lock().back.set_display_bus(slave);
    }

    /// Channel `index`'s output window: the rectangle of the managed area its output shows, as
    /// (x, y from the top, width, height); None for a channel that does not exist or does not run. The option
    /// board's channel is the one after the display's.
    pub fn channel_window(&self, index: usize) -> Option<(u32, u32, u32, u32)> {
        self.sh.view.lock().windows.get(index).copied().flatten()
    }

    /// `channel_window` for a channel named as a request names it: a display channel's number, or the option
    /// board's index or VOC id.
    pub fn channel_window_of(&self, channel: u32) -> Option<(u32, u32, u32, u32)> {
        self.sh.view.lock().window_of(channel)
    }

    /// `channel_scanout` for a channel named as a request names it.
    pub fn channel_scanout_of(&mut self, channel: u32) -> Option<(u32, u32, Vec<u32>)> {
        let index = self.sh.view.lock().index_of(channel)?;
        self.channel_scanout(index)
    }

    /// The pixels of `channel_window`, `0x00RRGGBB` in rows from the top, with its size: what the channel's
    /// output shows, for a device that takes the picture (the option board). Composes the frame as `compose`.
    pub fn channel_scanout(&mut self, index: usize) -> Option<(u32, u32, Vec<u32>)> {
        self.catch_up();
        let mut i = self.sh.inner.lock();
        let window = i.back.channel_window(index, i.core.managed)?;
        let (w, h, mut pixels) = i.window_pixels(window);
        if i.back.channel_blanked(index) {
            pixels.fill(0);
        }
        Some((w, h, pixels)).filter(|(w, h, _)| *w != 0 && *h != 0)
    }

    /// The frame the display would show, composed on the GPU and left there for a scan-out on the
    /// same device to take: no readback. None when the pipe has no GPU scan-out. The words are
    /// 0xAABBGGRR, rows from the top, and the buffer is rewritten by the next call.
    #[cfg(feature = "wgpu")]
    pub fn lend_frame(&mut self) -> Result<Option<LentFrame>, String> {
        if let Some(f) = self.fault_now() {
            return Err(f);
        }
        let lent = self.sh.inner.lock().back.lend_frame()?;
        Ok(lent.map(|(buffer, width, height)| LentFrame { buffer, width, height }))
    }

    /// A Crossbow link reset reached the widget: its crosstalk interface registers (widget control, interrupt
    /// destination and vectors) go back to their reset values, which the kernel's adaptor reset rewrites after
    /// it; the map RAM, the interrupt enable and every other register of the pipe stay (`Front::link_reset`,
    /// inferred; the KONA manual page documents it). A shell calls this for the link reset instead of a power-on.
    pub fn link_reset(&mut self) {
        self.sh.host.front.lock().link_reset();
        self.activity += 1;
        self.sh.report_widget();
    }

    /// The rows `first..end` of head `head`'s picture that may have changed since the last call for it: the rows
    /// the tiles drew, or all of them for anything else that can change the picture.
    pub fn take_damage(&mut self, head: u32) -> Option<(u32, u32)> {
        if let Some((lo, hi)) = self.pixels.take_damage() {
            for d in &mut self.damage {
                *d = Some(d.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
            }
        }
        let (_, y, _, h) = self.sh.view.lock().head(head)?.window?;
        let hull = self.damage.get_mut(head as usize)?.take()?;
        head_rows(hull, y, h)
    }

    /// The heads as they are now, for composing rows from any thread. None for an engine that has faulted.
    pub fn scan_source(&mut self) -> Option<ScanSource> {
        if self.dead() {
            return None;
        }
        let heads = self.sh.view.lock().heads.iter().map(|h| h.window.filter(|_| h.fits).map(|window| HeadView { window, blanked: h.blanked })).collect();
        Some(ScanSource { pixels: self.pixels.clone(), band: self.band.clone(), heads, sh: self.sh.clone() })
    }

    /// The sections of the last completed firmware load.
    pub fn load_record(&self) -> Vec<LoadSection> {
        self.sh.inner.lock().core.load_record().to_vec()
    }

    /// Stores waiting in the command stream.
    pub fn queued(&self) -> usize {
        self.pending()
    }

    /// A swap waits for its retrace.
    pub fn swap_waiting(&self) -> bool {
        self.sh.swap_wait.load(Acquire)
    }
}

impl<M: HostMem> Drop for KonaEngine<M> {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl<M: HostMem> BoardEngine for KonaEngine<M> {
    type Region = Region;

    fn write_at(&mut self, now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        self.sh.events.clock_ns.store(now_ns, Relaxed);
        self.write(off, bits, val)
    }

    /// One head for each running channel (the display channels, then the option board's), each showing its
    /// input rectangle of the managed area; one head, the whole managed area, while none runs.
    fn heads(&self) -> u32 {
        self.sh.view.lock().heads.len() as u32
    }

    fn scanout(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        out.clear();
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        let mut i = self.sh.inner.lock();
        // The size the picture has and the size `frame_size` answers are the same.
        self.sh.publish_view(&i);
        let Some(window) = i.head_window(head).filter(|&w| i.window_fits(w)) else { return Ok(None) };
        i.back.drain();
        let (_, _, w, h) = window;
        out.resize(w as usize * h as usize, 0);
        let blanked = i.head_blanked(head);
        i.pixels.window_rows(window, 0..h as usize, blanked, out, &mut i.band.lock());
        Ok((w != 0 && h != 0).then_some((w, h)))
    }

    /// A monitor channel's pixels reach the DAC as eight bits a colour (the colour map's ten bits and the gamma RAM's
    /// twelve are cut to eight on the way, established for the board's wiring), so they are widened by repeating
    /// their top bits; how the DAC widens its bypass input is unknown. The DAC cursor's colours are the one place
    /// a monitor head holds ten bits, and are given as they are. The option board's channel (the GVO) carries ten of
    /// the gamma RAM's twelve bits (`scanout_gvo`). Words are `x2b10g10r10`, red low.
    fn scanout10(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        if self.sh.view.lock().head(head).is_some_and(|h| h.option) {
            out.clear();
            if let Some(f) = self.fault_now() {
                return Err(Fault(f));
            }
            return self.sh.inner.lock().scanout_gvo(head, out);
        }
        let size = self.scanout(head, out)?;
        let widen = |c: u32| (c << 2) | (c >> 6);
        out.iter_mut().for_each(|p| *p = widen(*p >> 16 & 0xff) | widen(*p >> 8 & 0xff) << 10 | widen(*p & 0xff) << 20);
        // The DAC cursor is a position on the whole screen: a head that shows only a window of it has none drawn.
        if let (Some((w, h)), true) = (size, self.sh.view.lock().head(head).is_some_and(|h| h.whole)) {
            self.sh.inner.lock().back.draw_dac_cursor_wide(out, w as usize, w as usize, h as usize);
        }
        Ok(size)
    }

    /// The monitor channels' output is 8 bits; the option board's (a GVO) is 10 (kona `devices/dg/gvo-option.md`,
    /// the XMAP's 12-bit option connector and `0xc006` bits 10-9; established).
    fn head_bits(&self, head: u32) -> u32 {
        if self.sh.view.lock().head(head).is_some_and(|h| h.option) { 10 } else { 8 }
    }

    fn head_timing(&self, head: u32) -> Option<HeadTiming> {
        self.sh.view.lock().head(head)?.timing
    }

    fn frame_size(&self, head: u32) -> Option<(u32, u32)> {
        self.sh.view.lock().head(head)?.window.map(|(_, _, w, h)| (w, h))
    }

    fn frame_generation(&self, head: u32) -> u64 {
        if self.sh.view.lock().head(head).and_then(|h| h.window).is_none() {
            return 0;
        }
        1 + self.activity + self.sh.ran.load(Relaxed)
    }

    #[cfg(feature = "wgpu")]
    fn lend_frame(&mut self, head: u32) -> Result<Option<sgi_engine::LentFrame>, Fault> {
        if !self.sh.view.lock().head(head).is_some_and(|h| h.whole) {
            return Ok(None);
        }
        let generation = self.frame_generation(0);
        let lent = KonaEngine::lend_frame(self).map_err(Fault)?;
        Ok(lent.map(|f| sgi_engine::LentFrame { width: f.width, height: f.height, stride: f.width, words: sgi_engine::FrameWords::Abgr, generation, buffer: Arc::new(f.buffer) }))
    }

    #[cfg(feature = "wgpu")]
    fn compose_into(&mut self, head: u32, dst: &dyn std::any::Any, stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        if !self.sh.view.lock().head(head).is_some_and(|h| h.whole) {
            return Ok(None);
        }
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        self.sh.inner.lock().back.compose_into(dst, stride_words).map_err(Fault)
    }

    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write {
        self.activity += 1;
        if self.dead() {
            return Write::Fault;
        }
        let Some((words, n)) = Self::words(off, bits, val) else { return Write::Fault };
        let words = &words[..n];
        if self.pending() + Self::queues(words) > self.cap {
            self.flush();
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
        let take = if classify(port, true) == Access::Now { words.len() } else { words.len().min(self.cap.saturating_sub(self.pending())) };
        for &w in &words[..take] {
            self.store(port, w, self.source.charged);
        }
        if take < words.len() {
            self.flush();
        }
        self.notice_fault();
        take
    }

    fn read(&mut self, off: u32, bits: u32) -> Read {
        if self.dead() {
            return Read::Fault;
        }
        // A guest that polls with fewer than a full stage staged hands the worker its work here.
        self.flush();
        let at = [off, off.wrapping_add(4)];
        let at = match bits {
            32 => &at[..1],
            64 => &at[..],
            _ => return Read::Fault,
        };
        if at.iter().any(|&o| self.read_waits(o)) {
            return Read::Busy;
        }
        // The words that wait for the stream are read under one hold of the pipe, or the access is retried.
        let mut pipe = None;
        if at.iter().any(|&o| classify(o, false) != Access::Now) {
            match self.sh.inner.try_lock() {
                Some(g) => pipe = Some(g),
                None => return Read::Busy,
            }
        }
        let pending = self.pending();
        let mut value = 0u64;
        for &o in at {
            let word = match pipe.as_mut() {
                Some(i) if classify(o, false) != Access::Now => i.core.read_wait(&self.sh.host.front, o),
                _ => self.sh.read_now(o, pending),
            };
            value = value << 32 | word as u64;
        }
        Read::Value(value)
    }

    fn set_source(&mut self, source: Source) {
        self.source = source;
    }

    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool {
        if self.dead() {
            return false;
        }
        if write {
            Self::words(off, bits, 0).is_some_and(|(w, n)| self.pending() + Self::queues(&w[..n]) > self.cap)
        } else {
            match bits {
                32 => self.read_waits(off),
                64 => self.read_waits(off) || self.read_waits(off.wrapping_add(4)),
                _ => false,
            }
        }
    }

    fn run(&mut self, max_ops: usize) -> usize {
        if self.worker.is_some() || self.dead() {
            return 0;
        }
        self.flush();
        self.sh.run(max_ops)
    }

    fn frame(&mut self, now_ns: u64) {
        self.sh.events.clock_ns.store(now_ns, Relaxed);
        self.activity += 1;
        {
            let mut i = self.sh.inner.lock();
            i.back.set_swap_ready_held(self.sh.swap_held.load(Acquire));
            i.back.frame();
            self.sh.swap_wait.store(i.back.swaps_waiting(), Release);
            self.sh.publish_view(&i);
        }
        // The swaps showed: a stream held at one goes on.
        let _q = self.sh.q.lock();
        self.sh.work_cv.notify_all();
        self.sh.idle_cv.notify_all();
    }

    fn advance(&mut self, now_ns: u64) -> Pump {
        self.sh.events.clock_ns.store(now_ns, Relaxed);
        self.flush();
        self.sh.events.out.rearm();
        self.notice_fault();
        Pump { idle: self.idle(), events: self.sh.events.out.pending(), next_deadline: None, irq_levels: self.irq_levels() }
    }

    /// Runs the stream a slice at a time, so a caller with no budget still makes progress; with a worker, waits for it.
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle {
        self.sh.events.clock_ns.store(now_ns, Relaxed);
        self.settle(Some(Duration::from_nanos(budget_ns)));
        self.idle()
    }

    /// KONA signals by packets only: no line has a level.
    fn irq_levels(&self) -> u64 {
        0
    }

    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize) {
        self.sh.events.out.drain(out, max);
    }

    fn observe(&mut self, _region: &Region) -> Result<(), Fault> {
        if let Some(f) = self.fault_now() {
            return Err(Fault(f));
        }
        self.catch_up();
        self.sh.inner.lock().back.drain();
        Ok(())
    }

    fn observe_all(&mut self) -> Result<(), Refused> {
        self.need_parked()?;
        // The open GPU batch is submitted and waited for, then every plane is read back, so a completion fence
        // built on this means the GPU is done.
        self.sh.inner.lock().back.sync_all().map_err(Refused::Faulted)
    }

    /// Wall time the engine's own threads have spent drawing: the tile pool's workers, and the one that runs the
    /// stream when there is one, each summed. The inline stream runs inside calls.
    fn busy_ns(&self) -> u64 {
        let tiles = match self.sh.inner.try_lock() {
            Some(i) => {
                let t = i.back.busy_ns();
                self.sh.tile_ns.store(t, Relaxed);
                t
            }
            None => self.sh.tile_ns.load(Relaxed),
        };
        tiles + self.sh.busy_ns.load(Relaxed)
    }

    fn swaps_dropped(&self) -> u64 {
        self.sh.events.out.swaps_dropped()
    }

    fn events_waiting(&self) -> Option<Arc<AtomicBool>> {
        Some(self.sh.events.out.waiting())
    }

    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused> {
        let budget = (budget_ns != u64::MAX).then(|| Duration::from_nanos(budget_ns));
        if !self.settle(budget) && !self.dead() {
            return Err(Refused::Midway(if self.stalled() { "the stream waits at a swap for a frame".into() } else { format!("{} stores are still queued", self.pending()) }));
        }
        self.stop_worker();
        self.sh.inner.lock().back.quiesce();
        self.parked = true;
        match self.fault_now() {
            Some(f) => Err(Refused::Faulted(f)),
            None => Ok(()),
        }
    }

    fn resume(&mut self) {
        self.parked = false;
        self.sh.inner.lock().back.resume();
        self.spawn();
    }

    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused> {
        self.need_parked()?;
        let mut w = stream::Writer::default();
        let mut i = self.sh.inner.lock();
        self.sh.host.front.lock().visit(&mut w);
        i.core.visit(&mut w);
        i.back.visit_small_open(&mut w).map_err(Refused::Midway)?;
        i.core.visit_open(&mut w);
        self.sh.credits.lock().visit(&mut w);
        i.back.visit_swaps(&mut w);
        i.back.visit_option(&mut w);
        self.sh.host.front.lock().visit_xg(&mut w);
        i.back.visit_sync(&mut w);
        let meta = Meta { version: STATE_VERSION, state_len: w.out.len() as u32, bulk: i.back.bulk_layout() };
        let put = |sink: &mut dyn StateSink, name: &str, index: u64, bytes: Vec<u8>| sink.put(Chunk { name: name.into(), index, bytes }).map_err(|e| e.0);
        let sent = (|| {
            put(sink, "meta", 0, postcard::to_allocvec(&meta).map_err(|e| e.to_string())?)?;
            for (n, c) in w.out.chunks(CHUNK_SIZE).enumerate() {
                put(sink, "state", n as u64, c.to_vec())?;
            }
            i.back.save_stream(&mut |name, index, bytes| put(sink, name, index, bytes))
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
        let with_option = meta.version >= 5;
        let with_xg = meta.version >= 6;
        let with_sync = meta.version >= 7;
        if meta.state_len as usize > 1 << 28 {
            return Err(bad(format!("a state blob of {} bytes", meta.state_len)));
        }
        let mut i = self.sh.inner.lock();
        let mut loader = i.back.bulk_loader(&meta.bulk).map_err(bad)?;
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
        let prepared = i.back.finish_loader(loader).map_err(bad)?;
        {
            let cfg = i.core.cfg.clone();
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
                if with_option {
                    KonaBackend::check_option(&mut r);
                }
                if with_xg {
                    Front::new(&cfg).visit_xg(&mut r);
                }
                if with_sync {
                    KonaBackend::check_sync(&mut r);
                }
            } else {
                KonaBackend::check_small(&mut r);
            }
            r.finish().map_err(bad)?;
        }
        let mut r = stream::Reader::new(&blob);
        self.sh.host.front.lock().visit(&mut r);
        i.core.visit(&mut r);
        let mut credits = Credits::default();
        if open {
            i.back.visit_small_open(&mut r).map_err(bad)?;
            i.core.visit_open(&mut r);
            if by_source {
                credits.visit(&mut r);
            } else {
                credits.visit_scalar(&mut r);
            }
            if swaps {
                i.back.visit_swaps(&mut r);
            } else {
                i.back.clear_swaps();
            }
            if with_option {
                i.back.visit_option(&mut r);
            } else {
                i.back.restore_option(false);
            }
            if with_xg {
                self.sh.host.front.lock().visit_xg(&mut r);
            } else {
                self.sh.host.front.lock().clear_xg();
            }
            if with_sync {
                i.back.visit_sync(&mut r);
            } else {
                i.back.clear_sync();
            }
        } else {
            i.back.visit_small(&mut r).map_err(bad)?;
        }
        r.finish().map_err(bad)?;
        {
            let mut c = self.sh.credits.lock();
            *c = credits;
            self.sh.publish(&mut c, &i);
        }
        i.back.apply_chunks(prepared).map_err(bad)?;
        self.sh.host.activity.store(false, Relaxed);
        self.sh.events.out.post(Event::Irq);
        i.core.flash.write_through();
        self.sh.publish_view(&i);
        self.sh.ran.fetch_add(1, Relaxed);
        drop(i);
        self.sh.report_widget();
        Ok(())
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    /// Power-on state. The flash and the configuration stay.
    fn reset(&mut self) -> Result<(), Refused> {
        self.activity += 1;
        if self.dead() {
            // The stores the fault left behind are dropped with it.
            self.flush();
            self.sh.abandon();
        }
        if self.worker.is_some() || self.pending() != 0 {
            return Err(Refused::NotQuiesced);
        }
        let mut i = self.sh.inner.lock();
        i.core.reset();
        // A new epoch before the back end cancels its tickets.
        self.sh.host.front.lock().advance_epoch();
        i.back.reset();
        self.sh.host.front.lock().reset();
        self.sh.host.activity.store(false, Relaxed);
        {
            let mut c = self.sh.credits.lock();
            *c = Credits::default();
            self.sh.publish(&mut c, &i);
        }
        self.sh.publish_view(&i);
        drop(i);
        self.sh.poke.store(false, Release);
        self.sh.events.out.clear();
        self.sh.events.swaps.store(0, Relaxed);
        *self.sh.fault.lock() = None;
        self.sh.dead.store(false, Release);
        self.sh.events.out.post(Event::Irq);
        self.sh.report_widget();
        Ok(())
    }

    fn fault(&self) -> Option<String> {
        self.fault_now()
    }

    fn fail(&self, why: &str) {
        self.sh.fail(why);
    }
}
