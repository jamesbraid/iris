//! What an emulator's shell sees of a board engine: the types and the trait
//! every board's engine (MGRAS here; VPro and KONA next) implements.
//!
//! The shell owns buses, interrupts, clocks, locking policy and
//! persistence. The engine owns everything from the board's command stream
//! down. Threads, locks and calls are in the module docs of `dev::mgras::engine`.

use std::fmt;

/// A device fault: the engine stopped and the guest should see an error
/// state. Sticky until `reset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault(pub String);

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A DMA access that reached no RAM. What an engine does about it is the
/// board's: MGRAS reports `Event::BusFault` and goes on; KONA withholds the
/// DMA's completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault;

/// Why `save`, `load`, `reset` or `observe_all` did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Workers run or work is queued: `quiesce` first.
    NotQuiesced,
    /// The board is between the words of a command, or holds a swap for the
    /// retrace: ask again after more work or a `frame`. MGRAS saves anywhere.
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

/// A named piece of saved state: a small blob, or one piece of a large
/// store (`index` says which). A sink gets them in any order and a source
/// gives them back in any order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    pub name: String,
    pub index: u64,
    pub bytes: Vec<u8>,
}

/// Where `save` puts its chunks: a file-backed store, a migration stream,
/// or `Vec<Chunk>`.
pub trait StateSink {
    fn put(&mut self, chunk: Chunk) -> Result<(), Fault>;
}

/// Where `load` takes chunks from. `None` ends the state.
pub trait StateSource {
    fn next(&mut self) -> Result<Option<Chunk>, Fault>;
}

impl StateSink for Vec<Chunk> {
    fn put(&mut self, chunk: Chunk) -> Result<(), Fault> {
        self.push(chunk);
        Ok(())
    }
}

/// Chunks as bytes, for a shell that keeps state as one blob.
pub fn encode_chunks(c: &[Chunk]) -> Result<Vec<u8>, Fault> {
    postcard::to_allocvec(c).map_err(|e| Fault(format!("encode state: {e}")))
}

pub fn decode_chunks(b: &[u8]) -> Result<Vec<Chunk>, Fault> {
    postcard::from_bytes(b).map_err(|e| Fault(format!("decode state: {e}")))
}

/// A source over chunks held in memory.
pub struct VecSource(pub std::vec::IntoIter<Chunk>);

impl StateSource for VecSource {
    fn next(&mut self) -> Result<Option<Chunk>, Fault> {
        Ok(self.0.next())
    }
}

/// Where a store comes from, for flow control. `charged` is false for a
/// store that took no credit (a test's), so the count never runs past zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Source {
    pub id: u32,
    pub charged: bool,
}

/// Byte order of an access. Memory is the guest's: byte `i` of an access is
/// the byte at `addr + i`, and an engine reads words from it big-endian, as
/// the guest bus presents them. `Swap` reverses the bytes within each unit
/// of that width on the way, for a bridge that swaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Swap {
    None = 0,
    U16 = 1,
    U32 = 2,
    U64 = 3,
}

/// Host memory as the shell lends it to the engine.
///
/// `ram_*` is the lock-free path engine threads use: it reaches RAM only,
/// takes no emulator lock, and a target in MMIO or unmapped space is a
/// `MemFault`. `bus_*` may reach MMIO and may take emulator locks, so the
/// engine calls it only on the thread inside `write`, `read`, `run`,
/// `advance` or `wait_idle`. An engine that never pulls from MMIO never
/// calls it; the default sends it down the RAM path.
pub trait HostMem: Send + Sync + 'static {
    fn ram_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault>;
    fn ram_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault>;
    fn bus_read(&self, addr: u64, out: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_read(addr, out, swap)
    }
    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.ram_write(addr, data, swap)
    }
}

/// Board memory the shell owns and lends to an engine that does not
/// allocate its own (VPro's 128 MB, R300's 128 or 256 MB). Aligned to 8
/// bytes, host-endian 64-bit words, valid for the engine's life: the engine
/// drops its borrow on `reset` and on drop. Stores through it do not mark
/// the shell's dirty bits, so the engine reports what it wrote through
/// `drain_dirty`.
#[derive(Clone, Copy, Debug)]
pub struct Lent {
    pub ptr: *mut u8,
    pub len: usize,
}

// SAFETY: the lender promises the memory outlives the engine and that only
// the engine writes it while the engine runs.
unsafe impl Send for Lent {}
unsafe impl Sync for Lent {}

/// A run of lent memory the engine wrote, in bytes from its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRun {
    pub offset: u64,
    pub len: u64,
}

/// Outcome of a register or FIFO write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Done,
    /// The queue is full and nothing was taken: `run` or `wait_idle`, then
    /// retry.
    Busy,
    Fault,
}

/// Outcome of a register read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Value(u64),
    /// The answer depends on queued commands that have not run. Never
    /// waits on the GPU. A shell that cannot retry bounds a
    /// `wait_idle` and reads again.
    Busy,
    Fault,
}

/// What `advance` tells the shell's pump.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Pump {
    /// Nothing is queued and nothing runs.
    pub idle: bool,
    /// Events wait in the outbox.
    pub events: bool,
    /// When the engine next needs `advance`, in the shell's time base (ns,
    /// `u64`). MGRAS has no timers, so always `None`; VPro's retrace and
    /// KONA's frame are not.
    pub next_deadline: Option<u64>,
}

/// A packet the guest's routing registers composed: an interrupt for a
/// widget, as the crosstalk fabric carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub widget: u8,
    pub addr: u64,
    pub vector: u8,
    pub level: bool,
}

/// What the board signals, in the order it happened, but for damage, which
/// is joined per store and posted at the end of each run of work. Engines
/// add variants, hence `non_exhaustive`; MGRAS emits the first five, VPro
/// and KONA the last two.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The HQ flags changed (`flags()`): the shell derives interrupt levels
    /// from them. One event until drained.
    Flags,
    /// Drawing landed in a buffer, joined with earlier damage to the same
    /// store and planes.
    Damage(crate::display::Mark),
    /// Something not modelled yet, once per distinct text.
    Note(String),
    /// A DMA access reached no RAM.
    BusFault { addr: u64, write: bool },
    /// The engine stopped; see `fault()`.
    DeviceFault(String),
    /// An interrupt packet for the shell to deliver.
    Interrupt(Interrupt),
    /// Retired stores, by the source `set_source` named when they were
    /// written: the shell returns that many flow-control credits to it.
    Credits { source: u32, retired: u32 },
}

/// A mark that says "anything may have changed": what damage degrades to
/// when too many distinct stores wait.
pub const DAMAGE_ANY: u64 = u64::MAX;

/// The calls every board engine has. See the module docs of the MGRAS
/// engine for the rules each call keeps.
pub trait BoardEngine {
    /// A pixel box in the board's own terms: MGRAS names a buffer, VPro a
    /// plane (colour A or B, depth, stencil, window id, overlay).
    type Region;

    /// A register or FIFO write at a host-window offset; `bits` is 8, 16, 32
    /// or 64, and a board takes the widths its registers have.
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
    /// Whether `write` or `read` of `off` would wait or be refused right
    /// now: a write when the queue is full, a read that depends on queued
    /// work. For a shell that decides before calling, as QEMU's head-work
    /// slice does.
    fn access_waits(&self, off: u32, bits: u32, write: bool) -> bool;

    /// Inline mode: run queued work, at most `max_ops` operations; returns
    /// how many ran. Threaded mode with a worker running: runs nothing and
    /// returns 0.
    fn run(&mut self, max_ops: usize) -> usize;
    /// A frame boundary of the shell's scan-out timing: retrace requests
    /// complete, held swaps latch, in that order. For boards whose retrace
    /// is the shell's (KONA's per pipe, MGRAS's through its display chips); a
    /// board that times its own retrace names it in `Pump::next_deadline`
    /// instead and ignores this.
    fn frame(&mut self, _now_ns: u64) {}
    /// The pump: `now_ns` is the shell's time. Hands queued work to the
    /// worker, ends the run of work so far (an engine with a GPU unit submits
    /// what it has recorded), collects completions and returns what the
    /// shell needs next. Never executes queued work and never waits.
    fn advance(&mut self, now_ns: u64) -> Pump;
    /// The idle-wait input: the shell waits for the engine's CPU work to
    /// finish, at `now_ns`, for at most `budget_ns` of host time. Inline
    /// mode runs the queue instead. Returns whether the engine is idle. A
    /// replay records the call.
    fn wait_idle(&mut self, now_ns: u64, budget_ns: u64) -> bool;
    fn drain_events(&mut self, out: &mut Vec<Event>);
    /// The runs of lent memory written since the last call. Engines that
    /// own their memory report none.
    fn drain_dirty(&mut self, _out: &mut Vec<DirtyRun>) {}

    /// Bring a box of pixels current for a CPU reader. Waits for queued CPU
    /// work, then only for GPU batches that touch the box.
    fn observe(&mut self, region: &Self::Region) -> Result<(), Fault>;
    /// The same for all memory (save, reset, screendump). Needs quiescence.
    fn observe_all(&mut self) -> Result<(), Refused>;

    /// Run everything queued and park the workers. Before `observe_all`,
    /// `save`, `load` and `reset`. Unbounded: a shell that must not wait
    /// bounds a `wait_idle` first.
    fn quiesce(&mut self) -> Result<(), Fault>;
    /// Start the workers again.
    fn resume(&mut self);
    /// Stream the state to `sink` as chunks: small state as blobs, a large
    /// store a chunk a piece, all-zero pieces left out. Lent memory is not
    /// in it: the shell saves that as RAM. Needs quiescence.
    fn save(&mut self, sink: &mut dyn StateSink) -> Result<(), Refused>;
    /// Take a state back. It is checked whole before anything changes, so a
    /// state that fails leaves the engine as it was.
    fn load(&mut self, src: &mut dyn StateSource) -> Result<(), Refused>;
    /// Power-on state; clears a fault. Needs quiescence.
    fn reset(&mut self) -> Result<(), Refused>;

    /// The device-fault message, if the engine stopped.
    fn fault(&self) -> Option<String>;
}
