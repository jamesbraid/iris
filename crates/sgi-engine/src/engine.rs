//! The trait every board engine implements and the types that go with it.

use std::fmt;

use crate::mem::DirtyRun;
use crate::state::{StateSink, StateSource};

/// A device fault: the engine stopped and the guest should see an error
/// state. Sticky until `reset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault(pub String);

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why `quiesce`, `observe_all`, `save`, `load` or `reset` did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Workers run or work is queued: `quiesce` first.
    NotQuiesced,
    /// The engine cannot get there yet: it is stalled on a `frame` or a
    /// time (see `Idle`), or between the words of a command, or the budget
    /// ran out. The engine is still running; do what `wait_idle` says and
    /// ask again.
    Midway(String),
    /// The engine stopped; see `fault()`.
    Faulted(String),
    /// The state does not fit this engine: another board, version or size.
    Mismatch(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotQuiesced => f.write_str("the engine is not quiesced"),
            Refused::Midway(m) | Refused::Faulted(m) | Refused::Mismatch(m) => f.write_str(m),
        }
    }
}

/// Outcome of a register or FIFO write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Done,
    /// The queue is full and nothing was taken. Overflow protection for a
    /// guest that honours its credits; a shell that cannot refuse drives the
    /// engine (`shell::drain_stalls`) and retries.
    Busy,
    Fault,
}

/// Outcome of a register read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Value(u64),
    /// The answer depends on queued work that has not run. Never waits on
    /// the GPU. A shell that cannot retry (QEMU's bus) drives the engine
    /// with `shell::drain_stalls`, with no bound, and reads again: the
    /// engine never waits for anything but the shell's `frame` or time,
    /// which `drain_stalls` supplies, so the wait ends when the work does.
    Busy,
    Fault,
}

/// What a stalled engine waits for. Only the shell can give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallOn {
    /// A frame boundary: the shell calls `frame(now)`.
    Frame,
    /// This time, in the shell's time base: the shell calls `advance(t)`
    /// with `t` at least this.
    Time(u64),
}

/// Whether the engine has work, and if it cannot go on, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// Nothing is queued and nothing runs.
    Yes,
    /// Work is queued or running and the engine can finish it alone.
    Working,
    /// Work waits for an input only the shell gives. The engine never
    /// blocks on it inside any call; it says so here.
    Stalled(StallOn),
    /// The engine stopped (`fault()`).
    Faulted,
    /// The command stream stopped for good, as a fetch fault stops a command
    /// processor, and no queued work will run: only the guest's own reset
    /// of the stream, or `reset`, starts it again. Not a device fault: the
    /// engine still answers every call, `quiesce` parks it, and `save`,
    /// `observe_all` and the events work. A board whose streams do not stop
    /// never answers it.
    Stopped,
}

/// What `advance` tells the shell's pump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pump {
    pub idle: Idle,
    /// Events wait in the outbox.
    pub events: bool,
    /// When the engine next needs `advance`, in the shell's time base (ns,
    /// `u64`); `None` for a board with no timers of its own. A board with
    /// several timers answers the earliest; `head_deadline` gives each
    /// scan-out head's own.
    pub next_deadline: Option<u64>,
    /// `irq_levels()` as of this call.
    pub irq_levels: u64,
}

/// Where a store comes from, for flow control. The id is the shell's: the
/// engine echoes it in `Event::Credits` and never interprets it, so a KONA
/// hub numbered 0 is as real as any other. `charged` is false for a store
/// that took no credit (a test's), so the count never runs past zero; id 0
/// with `charged` false is a store with no sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Source {
    pub id: u32,
    pub charged: bool,
}

/// A packet the guest's routing registers composed: an interrupt for a
/// widget, as the crosstalk fabric carries it. `sender` is the widget it
/// comes from (the board's own widget, or the hub a KONA store came from).
/// Sender, widget, address and vector name the line the packet sets or
/// clears; `Outbox` keeps the newest packet for each line.
///
/// Contract rule: lines and receiver bits correspond one to one. Two lines
/// that reach one bit (a receiver that decodes the vector alone, for two
/// packets of different sender or address) can be reordered by the merge, and
/// the bit then ends in the wrong state. A board must not emit such lines;
/// `outbox::aliasing_lines` is the check, and the conformance suite runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub sender: u8,
    pub widget: u8,
    pub addr: u64,
    pub vector: u8,
    pub level: bool,
}

/// A box of pixels, corners included, in a store's own coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    pub fn union(&self, o: &Rect) -> Rect {
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
}

/// Drawing landed in `planes` of the pixels of `store` inside `rect`. What a
/// store and a plane are is the board's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mark {
    pub store: u64,
    pub planes: u64,
    pub rect: Rect,
}

/// A mark that says "anything may have changed": what damage degrades to
/// when too many distinct stores wait.
pub const DAMAGE_ANY: u64 = u64::MAX;

/// What the board signals. `Irq` and `Credits` coalesce, `Damage` joins,
/// `Note` and `BusFault` repeat once only: `Outbox` keeps the queue bounded.
/// `Interrupt` packets for one line (sender, widget, address, vector) collapse
/// to the newest. Engines add variants, hence `non_exhaustive`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The interrupt levels may have changed: read `irq_levels()`. One event
    /// until drained. Posted by every write that can change a level, and by
    /// `load` and `reset`.
    Irq,
    /// Drawing landed in a buffer, joined with earlier damage to the same
    /// store and planes.
    Damage(Mark),
    /// Something not modelled yet, once per distinct text.
    Note(String),
    /// A DMA access reached no RAM.
    BusFault { addr: u64, write: bool },
    /// The engine stopped; see `fault()`.
    DeviceFault(String),
    /// The GPU unit was lost (device removed, driver reset). The engine goes
    /// on without it or stops with a `DeviceFault`; the text names the
    /// adapter, for the shell's error.
    GpuLost(String),
    /// An interrupt packet for the shell to deliver. A packet replaces the
    /// queued one for the same line, so the queue holds at most one per line.
    /// The receiver sets or clears a pending bit, and nothing runs between
    /// two packets delivered in one batch, so only the last state is seen.
    /// An identical packet is dropped while its twin waits; once the shell has
    /// drained one, the next is queued again, so a clear in between is honoured.
    Interrupt(Interrupt),
    /// Retired stores, by the source `set_source` named when they were
    /// written: the shell returns that many flow-control credits to it. One
    /// event per source until drained.
    Credits { source: u32, retired: u32 },
    /// The pipe's own drive of the shared swap-ready line changed: `drive` is the new state of the bit its firmware
    /// writes to hold or release the line. A shell with several pipes combines them (wired AND) and tells each
    /// engine the result with its `set_swap_ready_held` (KONA). Every change is one event, in order.
    SwapReady { drive: bool },
    /// A buffer swap reached the board's swap point: `count` swaps so far (1 for the first, since the engine was built or
    /// reset), and `time_ns` the shell time the engine last had (its `write_at`, `advance`, `wait_idle` and `frame`
    /// calls) when it consumed the swap, so it can be late by the work between two pumps. Never coalesced: one event
    /// per swap, in order, up to a bound; beyond it the swap is dropped and counted (`BoardEngine::swaps_dropped`), the
    /// next event's `count` showing the gap.
    Swap { count: u64, time_ns: u64 },
}

/// How the words of a frame the GPU holds are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameWords {
    /// `0xAABBGGRR` with alpha set, red in the low byte: what a board's scan-out kernel writes and
    /// a presenter's texture reads. `lend_frame`'s buffer.
    Abgr,
    /// `0x00RRGGBB`: what `scanout` copies and `compose_into` writes.
    Rgb,
    /// `x2b10g10r10`, red in the low ten bits (DRM `XBGR2101010`, fourcc `XB30`): what `scanout10` copies
    /// and `compose_into10` writes.
    Xbgr2101010,
}

/// A composed frame left on the GPU for a scan-out on the same device to
/// read in place, with no readback.
///
/// `buffer` holds the board's `wgpu::Buffer` (an `Arc<wgpu::Buffer>`, as a
/// `dyn Any`, since this crate has no GPU dependency): the shell downcasts
/// it. The words are laid out as `words` says, `stride` words a row (at
/// least `width`), `width` by `height`, rows from the top. The buffer is
/// valid until the next `lend_frame` of the same head. `generation` is the
/// head's `frame_generation` as this frame was composed; 0 when the board
/// keeps none.
#[derive(Clone)]
pub struct LentFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub words: FrameWords,
    pub generation: u64,
    pub buffer: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

impl fmt::Debug for LentFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LentFrame").field("width", &self.width).field("height", &self.height).field("stride", &self.stride).field("words", &self.words).field("generation", &self.generation).finish_non_exhaustive()
    }
}

/// The scan-out timing of a head, for a shell that paces its own retrace by the board's.
///
/// A frame is `frame_clocks` clocks of `clock_hz`, in `fields` fields (retrace n falls at
/// `epoch + n * frame_clocks / (fields * clock_hz)`), over `lines` lines in all. `epoch_ns` is when retrace 0
/// fell in the shell's `advance` time base, or -1 where the shell's clock starts the phase (a board whose retrace
/// is the shell's, as KONA's is). `generation` changes whenever the timing or the phase restarts, so a shell
/// reads it, and the rest only when it moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadTiming {
    pub frame_clocks: u64,
    pub clock_hz: u32,
    pub fields: u32,
    pub lines: u32,
    pub epoch_ns: i64,
    pub generation: u64,
}

/// The calls every board engine has. A board's constructor is not in the
/// trait: each board takes its own configuration. The rules every call
/// keeps are in the crate docs.
pub trait BoardEngine {
    /// A pixel box in the board's own terms: MGRAS names a buffer, VPro a
    /// plane, KONA a (word, rect).
    type Region;

    /// A register or FIFO write at a host-window offset; `bits` is 8, 16, 32
    /// or 64. A board answers `Fault` for a width its registers do not have,
    /// or says how it widens one (MGRAS takes 8 and 16 as 32: its shell sets
    /// `min_access_size` to 4).
    fn write(&mut self, off: u32, bits: u32, val: u64) -> Write;
    /// FIFO words in a batch, in order (an 8-byte store is two words); the
    /// number taken (all, or what the queue had room for). A charged source
    /// earns one credit for each word the board retires, so a shell with
    /// 8-byte stores that wants one credit for the pair uses `write`.
    ///
    /// Only the ports a board's FIFOs have: a board whose guest pushes
    /// through register apertures inside the window (a push mode) has no
    /// port, answers 0 and notes it, and takes those words through `write`.
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize;
    fn read(&mut self, off: u32, bits: u32) -> Read;
    /// Name where the stores that follow come from, for flow control: the
    /// engine returns credits to that source as `Event::Credits`. Each
    /// credit goes to the source of the store that earned it: the credits of
    /// a packet still arriving wait for its last word, and a credit target
    /// the driver holds parked keeps credits until the write that releases
    /// it, and both are kept by source, so stores of one packet through two
    /// Hubs are paid back to their own. That a credit returns to the Hub
    /// that sent its store is a software expectation (the IRIX kernel charges
    /// the storing processor's interface and waits for that count to return) and
    /// the KONA page's statement; that a packet's credits wait for its last
    /// word, and how a parked target holds them, are inferred (the pipe's own
    /// rule is unknown). Boards without credits ignore it.
    fn set_source(&mut self, _source: Source) {}
    /// `write` with the shell's time (ns, its time base) at the store, for a
    /// register whose effect starts a timer at the moment of the write (a
    /// scan-out mode: the retrace phase restarts then, not at the next
    /// `advance`). The default is `write`. Only a store the board takes inside
    /// the call has the time; one queued behind other work takes effect when
    /// it runs, and anchors at the next `advance`.
    fn write_at(&mut self, _now_ns: u64, off: u32, bits: u32, val: u64) -> Write {
        self.write(off, bits, val)
    }
    /// Whether `write` or `read` of `off` would wait or be refused now: a
    /// write when the queue is full, a read that depends on queued work.
    /// False on a faulted engine: the call answers `Fault` at once.
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool;

    /// Inline mode: run queued work, at most `max_ops` operations, stopping
    /// at a stall; returns how many ran. With a worker running or after a
    /// fault: runs nothing and returns 0.
    fn run(&mut self, max_ops: usize) -> usize;
    /// A frame boundary of the shell's scan-out timing: held swaps latch and
    /// retrace requests complete, in the order the board's hardware does (the
    /// board's manual page says which: KONA shows the swap first, then
    /// completes the requests waiting), and an engine stalled on
    /// `StallOn::Frame` goes on. A swap with nothing queued behind it leaves
    /// the engine `Idle::Yes`: the display changes at the next `frame`, which
    /// the shell keeps calling at every retrace. For boards whose retrace is the shell's
    /// (KONA's per pipe); a board that times its own retrace names it in
    /// `Pump::next_deadline` and ignores this. MGRAS ignores it.
    fn frame(&mut self, _now_ns: u64) {}
    /// The number of scan-out heads (CRTCs) `scanout` and `lend_frame` take;
    /// 0 for a board that composes no picture through the trait.
    fn heads(&self) -> u32 {
        0
    }
    /// The composed picture of `head`, `0x00RRGGBB` a pixel, rows from the
    /// top, copied into `out` (cleared first); `(width, height)`, or `None`
    /// while the head has no mode, or no such head. Brings the pixels it shows
    /// current as `observe` does.
    fn scanout(&mut self, _head: u32, _out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        Ok(None)
    }
    /// `scanout` with ten bits a colour, in `x2b10g10r10` (`XB30`: red in the low ten bits, then green, then blue,
    /// the top 2 bits zero): the top 8 bits of each are what `scanout` gives. A board whose colour maps carry only
    /// 8 bits widens them by repeating their top bits, so the low two bits add no precision; a board with wider
    /// maps gives the extra (`head_bits` says which).
    fn scanout10(&mut self, head: u32, out: &mut Vec<u32>) -> Result<Option<(u32, u32)>, Fault> {
        let size = self.scanout(head, out)?;
        let widen = |c: u32| (c << 2) | (c >> 6);
        out.iter_mut().for_each(|p| *p = widen(*p & 0xff) << 20 | widen(*p >> 8 & 0xff) << 10 | widen(*p >> 16 & 0xff));
        Ok(size)
    }
    /// The bits a colour that `head`'s hardware output carries: 8 unless the board's maps and DAC keep more
    /// (VPro and MGRAS 10, a KONA GVO channel 10; a KONA monitor channel 8). Where this is above 8,
    /// `scanout10` gives real extra bits; otherwise its low two bits repeat the top ones. Cheap; waits for nothing.
    fn head_bits(&self, _head: u32) -> u32 {
        8
    }
    /// The same picture left on the GPU (`LentFrame`) instead of copied to the
    /// host; `None` where the board has no GPU scan-out (use `scanout`).
    fn lend_frame(&mut self, _head: u32) -> Result<Option<LentFrame>, Fault> {
        Ok(None)
    }
    /// The same picture composed straight into the caller's buffer (`dst`: an `Arc<wgpu::Buffer>` or
    /// `wgpu::Buffer` on the board's device, as a `dyn Any`) as `0x00RRGGBB`, `stride_words` a row, rows
    /// from the top: the buffer a display exports is then the compose target, with no second pass.
    /// `(width, height)`; `None` where the board has no GPU scan-out, no such head or no frame. A
    /// buffer too small for `(height - 1) * stride_words + width` words, or a stride under the width, is an `Err` that leaves the engine and its GPU running (not a device fault: `fault()` stays `None`).
    fn compose_into(&mut self, _head: u32, _dst: &dyn std::any::Any, _stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        Ok(None)
    }
    /// `compose_into` with ten bits a colour: `scanout10`'s words (`FrameWords::Xbgr2101010`), word for word,
    /// with the same destination rules, sizes and errors. `None` where the board has no wide GPU compose (use
    /// `compose_into`, or `scanout10`).
    fn compose_into10(&mut self, _head: u32, _dst: &dyn std::any::Any, _stride_words: u32) -> Result<Option<(u32, u32)>, Fault> {
        Ok(None)
    }
    /// Wall time, in ns, the engine's own worker threads have spent running work since it was built (not waiting
    /// for it), summed over them, so it can exceed wall time. Work done inside a call is in that call's time and
    /// not here; 0 for an engine that runs everything inside calls. Cheap; waits for nothing.
    fn busy_ns(&self) -> u64 {
        0
    }
    /// Swap events (`Event::Swap`) dropped because the outbox held too many unread; 0 for a board that posts none.
    fn swaps_dropped(&self) -> u64 {
        0
    }
    /// The timing of `head`'s scan-out (`HeadTiming`), `None` while none is loaded or for no such head: the
    /// shell keeps its default. Cheap; waits for nothing.
    fn head_timing(&self, _head: u32) -> Option<HeadTiming> {
        None
    }
    /// The size of the picture of `head` (`scanout`, `lend_frame` and `compose_into` give frames no larger):
    /// the video mode's, so a shell can size a display buffer without composing; `None` for no such head or
    /// while it has no mode. Cheap; waits for nothing.
    fn frame_size(&self, _head: u32) -> Option<(u32, u32)> {
        None
    }
    /// A number that changes whenever the picture of `head` may have: any store, retrace swap, CPU
    /// write, load or reset. A shell that composed at generation `g` and reads `g` again can skip the
    /// call. 0 for a board that keeps none (compose every time). Cheap; waits for nothing.
    fn frame_generation(&self, _head: u32) -> u64 {
        0
    }
    /// When `head` next needs `advance` (its next retrace), `None` for a board
    /// whose head has no timer of its own.
    fn head_deadline(&self, _head: u32) -> Option<u64> {
        None
    }
    /// The pump: `now_ns` is the shell's time. Hands queued work to the
    /// worker, ends the run of work so far (an engine with a GPU unit submits
    /// what it has recorded), completes what is due at `now_ns` (a stall on
    /// `StallOn::Time` ends here), then the write-backs, the credits and the
    /// interrupts that follow from them, in that order. Never executes queued
    /// work and never waits. A board that runs a store inside `write` (VPro
    /// inline) has nothing queued, and `run` returns 0.
    fn advance(&mut self, now_ns: u64) -> Pump;
    /// The idle-wait input: the shell waits for the engine's CPU work to
    /// finish, at `now_ns`, for at most `budget_ns` of host time. Inline mode
    /// runs the queue instead. Returns at once with `Stalled` when the work
    /// waits for the shell; `Working` means the budget ran out. A replay
    /// records the call.
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> Idle;
    /// The interrupt output lines as levels, one bit per line, from atomics
    /// only: callable at any time, never waits. Changes are announced with
    /// `Event::Irq`. A board that signals by packets has no level and answers 0.
    fn irq_levels(&self) -> u64;
    /// Move up to `max` events out, oldest first; the rest stay.
    fn drain_events(&mut self, out: &mut Vec<Event>, max: usize);
    /// The runs of lent memory written since the last call. Call it after
    /// `quiesce` as well, so the last writes are in it. Engines that own
    /// their memory report none.
    fn drain_dirty(&mut self, _out: &mut Vec<DirtyRun>) {}
    /// The ranges of lent memory the shell must not map straight into the
    /// guest: a CPU read of one calls `observe_lent` first, a CPU write
    /// calls `cpu_wrote` after. Fixed from construction to drop.
    fn trapped(&self, _out: &mut Vec<DirtyRun>) {}
    /// Bring `len` bytes of trapped lent memory at `off` current for a CPU
    /// read: waits for queued CPU work, then only for GPU work that touches it.
    fn observe_lent(&mut self, _off: u64, _len: u64) -> Result<(), Fault> {
        Ok(())
    }
    /// The CPU wrote `len` bytes of trapped lent memory at `off`: the
    /// engine takes the new words in place of any copy it holds.
    fn cpu_wrote(&mut self, _off: u64, _len: u64) {}

    /// Bring a box of pixels current for a CPU reader. Waits for queued CPU
    /// work, then only for GPU batches that touch the box.
    fn observe(&mut self, region: &Self::Region) -> Result<(), Fault>;
    /// The same for all memory (save, reset, screendump). Needs quiescence.
    fn observe_all(&mut self) -> Result<(), Refused>;

    /// Run everything queued and park the workers, for at most `budget_ns`
    /// of host time (`u64::MAX`: no bound). At least one slice of work runs
    /// before the budget is looked at, so `quiesce(0)` still makes progress
    /// (and so does `wait_idle`). Before `observe_all`, `save`,
    /// `load` and `reset`, and when the VM stops. A stalled engine, or one
    /// still busy at the budget, answers `Midway` and keeps running; `Faulted`
    /// still parks. Never hangs on a shell input.
    fn quiesce(&mut self, budget_ns: u64) -> Result<(), Refused>;
    /// Start the workers again.
    fn resume(&mut self);
    /// Stream the state to `sink` as chunks: `meta` first, small state as
    /// blobs, a large store a chunk a piece, all-zero pieces left out. Lent
    /// memory is not in it: the shell saves that as RAM. Needs quiescence.
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused>;
    /// Take a state back, applying chunks as they arrive. After an error
    /// the shell calls `reset`; an engine that checks first may leave its
    /// state untouched, and need not fault. Needs quiescence.
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused>;
    /// The version of the state `save` writes and `load` takes, so a shell
    /// can set its migration version and refuse an older stream cleanly.
    fn state_version(&self) -> u32;
    /// Power-on state; clears a fault. Needs quiescence (a faulted engine
    /// parks on `quiesce`, which answers `Faulted`). Drops the queued events
    /// and the credits still owed, so the shell resets its own credit counts
    /// with it, then posts one `Event::Irq`.
    fn reset(&mut self) -> Result<(), Refused>;

    /// The device-fault message, if the engine stopped.
    fn fault(&self) -> Option<String>;
    /// Stop the engine with a device fault from outside it, such as a panic
    /// in the shell's glue. The first fault stays.
    fn fail(&self, why: &str);
}
