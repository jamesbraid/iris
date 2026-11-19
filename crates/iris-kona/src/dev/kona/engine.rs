//! One KONA pipe's front end, token layer and drawing, for a shell that owns it whole.
//!
//! No thread of its own: the shell calls `write` and `read` for the bus, `run` for the command
//! stream and `frame` at each retrace it chooses. Interrupt packets and credits go to an outbox
//! the shell drains; host memory is the shell's `PipeMemory`. Iris's front end is mounted
//! unchanged. This file is iris's board routing without its ring and thread, plus Hub credits.

use std::collections::VecDeque;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use parking_lot::Mutex;

use super::back::snapshot::{self, KonaChunk};
use super::back::KonaBackend;
use super::defs::{self, *};
use super::flash::Flash;
use super::front::{Front, Host, InterruptPacket};
use super::handle::DisplayHandle;
use super::ident::KonaConfig;
use super::pipe::{Core, LoadSection};
use super::regs::{classify, Access};
use super::stream;
use crate::chunk_store::ChunkStore;
use crate::irq::InterruptController;
use crate::traits::{BusDevice, BUS_ERR, BUS_OK};
use crate::SharedGpu;

/// Host memory as the pipe's DMA reaches it: physical addresses before the
/// fabric adds the widget. Calls come from the engine's callers' threads
/// only, inside `write`, `read`, `run` and `frame`.
pub trait PipeMemory: Send + Sync {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
    fn write(&self, addr: u64, data: &[u8]) -> bool;
}

struct Mem(Arc<dyn PipeMemory>);

impl BusDevice for Mem {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> u32 {
        if self.0.read(addr, buf) { BUS_OK } else { BUS_ERR }
    }

    fn write_bytes(&self, addr: u64, data: &[u8]) -> u32 {
        if self.0.write(addr, data) { BUS_OK } else { BUS_ERR }
    }
}

/// Who sent a store, and whether the sender's interface charged it a graphics
/// credit. The Hub returns credits to the sender, and a store no processor
/// made (a test's) is never charged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    /// The sending Hub, or None for a store with no sender.
    pub hub: Option<u8>,
    pub charged: bool,
}

impl Source {
    /// A store no processor made.
    pub const UNCHARGED: Source = Source { hub: None, charged: false };
}

/// What the engine hands the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    /// An assert-only interrupt write: from `source`'s widget to `address`,
    /// vector `vector`.
    Interrupt { source: u8, address: u64, vector: u8 },
    /// `credits` graphics credits go back to `hub` (None: the store had no
    /// sender; the shell picks its default link).
    Credits { hub: Option<u8>, credits: u32 },
}

/// The outbox, and the hook that tells the shell something is in it.
struct Outbox {
    items: Mutex<Vec<Out>>,
    notify: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Outbox {
    fn push(&self, o: Out) {
        self.items.lock().push(o);
        if let Some(f) = &*self.notify.lock() {
            f();
        }
    }
}

impl InterruptController for Outbox {
    fn set_line(&self, _line: u32, _level: bool) {}

    fn packet(&self, source: u8, address: u64, line: u32, _level: bool) -> bool {
        self.push(Out::Interrupt { source, address, vector: line as u8 });
        true
    }
}

/// What an `Engine` is built with.
pub struct Options {
    /// The pixel store and the drawing on the GPU, when the shell has one.
    pub gpu: Option<SharedGpu>,
    /// Show a swap at the retrace that follows it, not when its command is reached.
    pub latch_swaps: bool,
    /// The most stores the command stream holds before `write` answers busy.
    pub queue: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { gpu: None, latch_swaps: true, queue: 1 << 16 }
    }
}

/// A read: the register's value, or the stream still owes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Value(u32),
    /// Stores wait in the command stream; call `run` and ask again.
    Wait,
}

/// A store: taken, or the stream is full and the same store is to be retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Done,
    Busy,
}

/// Whether `run` left work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Run {
    Idle,
    More,
}

/// What the shell may ask without changing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Stores waiting in the command stream.
    pub queued: usize,
    /// Words the framers hold of commands still arriving.
    pub held_words: u32,
    /// A swap waits for its retrace.
    pub swap_waiting: bool,
    /// Why the pipe stopped taking commands, if it did: its command stream failed, or a tile worker
    /// panicked.
    pub failed: Option<String>,
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
    last: Option<u8>,
}

pub struct Engine {
    host: Arc<Host>,
    core: Core,
    back: KonaBackend,
    stream: VecDeque<Stored>,
    cap: usize,
    out: Arc<Outbox>,
    credits: Credits,
    pixels: super::back::display::Pixels,
}

impl Engine {
    /// A pipe of configuration `cfg` reading and writing host memory through `mem`.
    pub fn new(cfg: KonaConfig, mem: Arc<dyn PipeMemory>, opts: Options) -> Result<Engine, String> {
        let out = Arc::new(Outbox { items: Mutex::new(Vec::new()), notify: Mutex::new(None) });
        let mut back = KonaBackend::with_gpu(cfg, opts.gpu)?;
        let pixels = back.pixels();
        let mut front = Front::new(&cfg);
        front.probe_value = cfg.probe;
        let host = Arc::new(Host::new(front, out.clone(), Arc::new(Mem(mem))));
        back.attach_manual(DisplayHandle::new(&host));
        back.latch_swaps(opts.latch_swaps);
        Ok(Engine { host, core: Core::new(cfg, Flash::fresh()), back, stream: VecDeque::new(), cap: opts.queue, out, credits: Credits::default(), pixels })
    }

    /// A hook that does nothing but schedule: it runs when something enters the outbox, from
    /// whichever thread put it there, and must not call back into the engine.
    pub fn set_notify(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.out.notify.lock() = Some(f);
    }

    /// Move what the engine has to say into `v`.
    pub fn drain_out(&self, v: &mut Vec<Out>) {
        v.append(&mut self.out.items.lock());
    }

    // ----- the bus ----------------------------------------------------------

    /// A 4-byte read at aperture offset `off`.
    pub fn read(&mut self, off: u32) -> Read {
        match classify(off, false) {
            Access::Now => Read::Value(self.read_now(off)),
            _ if !self.stream.is_empty() => Read::Wait,
            _ => Read::Value(self.core.read_wait(&self.host.front, off)),
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

    /// A store of `size` (4 or 8) bytes from `src` at aperture offset `off`. An 8-byte store to the
    /// command page is two words, high first, at the same offset; any other is two words at `off`
    /// and `off + 4`. All of a store's words go in or none does, so `Busy` can be retried.
    pub fn write(&mut self, off: u32, size: u32, val: u64, src: Source) -> Write {
        let words: [(u32, u32); 2];
        let words = match size {
            4 => {
                words = [(off, val as u32), (0, 0)];
                &words[..1]
            }
            8 => {
                let second = if off == COMMAND_PAGE { off } else { off.wrapping_add(4) };
                words = [(off, (val >> 32) as u32), (second, val as u32)];
                &words[..]
            }
            _ => return Write::Done,
        };
        let queues = words.iter().filter(|&&(o, _)| classify(o, true) != Access::Now).count();
        if self.stream.len() + queues > self.cap {
            return Write::Busy;
        }
        // The credit goes with the store's last word, the one that completes it.
        let n = words.len();
        for (i, &(o, v)) in words.iter().enumerate() {
            let counted = i + 1 == n && src.charged;
            if classify(o, true) == Access::Now {
                self.write_now(o, v, src);
                if counted {
                    self.settle(true, self.core.packets, src.hub);
                }
            } else {
                self.stream.push_back(Stored { off: o, val: v, src, counted });
            }
        }
        Write::Done
    }

    fn write_now(&mut self, off: u32, value: u32, src: Source) {
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
            self.give(src.hub, parked);
        }
        if off == INT_CAUSE && self.core.reasons_waiting() {
            self.core.reason_drain(&host);
        }
    }

    // ----- the command stream -----------------------------------------------

    /// Take up to `budget` stores out of the command stream and run them, in order.
    pub fn run(&mut self, budget: usize) -> Run {
        let host = self.host.clone();
        let mut worked = false;
        for _ in 0..budget {
            let Some(s) = self.stream.pop_front() else { break };
            worked = true;
            let before = self.core.packets;
            match classify(s.off, true) {
                Access::Waits => self.core.write_wait(&host, &mut self.back, s.off, s.val),
                _ => self.core.stream_store(&host, &mut self.back, s.off, s.val),
            }
            self.credits.last = s.src.hub.or(self.credits.last);
            self.settle(s.counted, before, s.src.hub);
        }
        self.core.reason_drain(&host);
        if worked && self.stream.is_empty() {
            // Credits the framers held while the stores were arriving go back once nothing holds them.
            self.settle(false, self.core.packets, self.credits.last);
            host.with_front(|f| ((), f.lowater()));
        }
        if self.stream.is_empty() { Run::Idle } else { Run::More }
    }

    /// The Hub's accounting after the pipe took a store's words (`before`: the packets completed
    /// before it).
    fn settle(&mut self, counted: bool, before: u64, hub: Option<u8>) {
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
    fn give(&mut self, hub: Option<u8>, credits: u32) {
        if credits == 0 {
            return;
        }
        if self.host.front.lock().low_window_write & 1 != 0 {
            self.credits.parked += credits;
            return;
        }
        self.out.push(Out::Credits { hub, credits });
    }

    // ----- time -------------------------------------------------------------

    /// A retrace, supplied by the shell's timing. A swap that waited shows first, then the retrace
    /// requests that waited complete (the tag register reads the last, the video cause rises and its
    /// interrupt packet goes to the outbox), then the frame is composed.
    pub fn frame(&mut self) {
        self.back.frame();
    }

    // ----- state ------------------------------------------------------------

    pub fn status(&self) -> Status {
        let poisoned = self.back.is_poisoned().then(|| "a tile worker panicked; the pipe is dead".to_string());
        Status { queued: self.stream.len(), held_words: self.core.held_words(), swap_waiting: self.back.swaps_waiting(), failed: self.core.failed.clone().or(poisoned) }
    }

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

    /// Run the whole command stream, then bring the drawing to rest: every queued raster job
    /// has run. The shell has stopped submitting. `resume` undoes it.
    pub fn quiesce(&mut self) {
        while self.run(1 << 12) == Run::More {}
        self.back.quiesce();
    }

    pub fn resume(&mut self) {
        self.back.resume();
    }

    /// Power-on state. The flash and the configuration stay.
    pub fn reset(&mut self) {
        self.stream.clear();
        self.core.reset();
        // A new epoch before the back end cancels its tickets.
        self.host.front.lock().advance_epoch();
        self.back.reset();
        self.host.front.lock().reset();
        self.host.activity.store(false, Relaxed);
        self.credits = Credits::default();
    }

    /// The board's state as chunks: the pixel store and the rest of the bulk state, and the small
    /// state as one more list named `state`. Quiesced, between commands, with no swap waiting.
    pub fn save_chunks(&mut self, store: &ChunkStore) -> std::io::Result<Vec<KonaChunk>> {
        self.quiesce();
        let io = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, e);
        if !self.stream.is_empty() {
            return Err(io("commands wait in the stream".into()));
        }
        self.core.at_boundary().map_err(io)?;
        let mut w = stream::Writer::default();
        self.host.front.lock().visit(&mut w);
        self.core.visit(&mut w);
        self.back.visit_small(&mut w).map_err(io)?;
        let mut chunks = self.back.save_chunks(store)?;
        chunks.push(KonaChunk { name: format!("state-{}", w.out.len()), hashes: snapshot::put_bytes(store, &w.out)? });
        Ok(chunks)
    }

    /// Restore what `save_chunks` wrote, into a pipe of the same configuration, all or nothing.
    pub fn load_chunks(&mut self, store: &ChunkStore, list: &[KonaChunk]) -> Result<(), String> {
        let (state, rest): (Vec<_>, Vec<_>) = list.iter().cloned().partition(|c| c.name.starts_with("state-"));
        let [c] = &state[..] else { return Err("a snapshot holds one state list".into()) };
        let n: usize = c.name["state-".len()..].parse().map_err(|_| "the state list's size is not a number")?;
        let blob = snapshot::get_bytes(store, &c.hashes, n)?;
        let prepared = self.back.prepare_chunks(store, &rest)?;
        {
            let cfg = self.core.cfg.clone();
            let mut r = stream::Reader::new(&blob);
            Front::new(&cfg).visit(&mut r);
            Core::new(cfg, Flash::fresh()).visit(&mut r);
            KonaBackend::check_small(&mut r);
            r.finish()?;
        }
        let mut r = stream::Reader::new(&blob);
        self.host.front.lock().visit(&mut r);
        self.core.visit(&mut r);
        self.back.visit_small(&mut r)?;
        r.finish()?;
        self.back.apply_chunks(prepared)?;
        self.host.activity.store(false, Relaxed);
        Ok(())
    }
}
