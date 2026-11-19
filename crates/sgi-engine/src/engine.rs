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
}

/// What `advance` tells the shell's pump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pump {
    pub idle: Idle,
    /// Events wait in the outbox.
    pub events: bool,
    /// When the engine next needs `advance`, in the shell's time base (ns,
    /// `u64`); `None` for a board with no timers of its own.
    pub next_deadline: Option<u64>,
    /// `irq_levels()` as of this call.
    pub irq_levels: u64,
}

/// Where a store comes from, for flow control. `charged` is false for a
/// store that took no credit (a test's), so the count never runs past zero.
/// A board with no source of its own (KONA's absent hub) gives it id 0.
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
    /// number taken (all, or what the queue had room for).
    fn push_words(&mut self, port: u32, words: &[u32]) -> usize;
    fn read(&mut self, off: u32, bits: u32) -> Read;
    /// Name where the stores that follow come from, for flow control: the
    /// engine returns credits to that source as `Event::Credits`. A packet's
    /// credits go to the source of the store that completed it, and a
    /// credit target the driver holds parked keeps credits until the write
    /// that releases them, which goes to that writer. Boards without
    /// credits ignore it.
    fn set_source(&mut self, _source: Source) {}
    /// Whether `write` or `read` of `off` would wait or be refused now: a
    /// write when the queue is full, a read that depends on queued work.
    /// False on a faulted engine: the call answers `Fault` at once.
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool;

    /// Inline mode: run queued work, at most `max_ops` operations, stopping
    /// at a stall; returns how many ran. With a worker running or after a
    /// fault: runs nothing and returns 0.
    fn run(&mut self, max_ops: usize) -> usize;
    /// A frame boundary of the shell's scan-out timing: retrace requests
    /// complete and held swaps latch, in that order, and an engine stalled on
    /// `StallOn::Frame` goes on. For boards whose retrace is the shell's
    /// (KONA's per pipe); a board that times its own retrace names it in
    /// `Pump::next_deadline` and ignores this. MGRAS ignores it.
    fn frame(&mut self, _now_ns: u64) {}
    /// The pump: `now_ns` is the shell's time. Hands queued work to the
    /// worker, ends the run of work so far (an engine with a GPU unit submits
    /// what it has recorded), completes what is due at `now_ns` (a stall on
    /// `StallOn::Time` ends here), then the write-backs, the credits and the
    /// interrupts that follow from them, in that order. Never executes queued
    /// work and never waits.
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
    /// of host time (`u64::MAX`: no bound). Before `observe_all`, `save`,
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
    /// Power-on state; clears a fault. Needs quiescence. Posts `Event::Irq`.
    fn reset(&mut self) -> Result<(), Refused>;

    /// The device-fault message, if the engine stopped.
    fn fault(&self) -> Option<String>;
    /// Stop the engine with a device fault from outside it, such as a panic
    /// in the shell's glue. The first fault stays.
    fn fail(&self, why: &str);
}
