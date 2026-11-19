//! What an emulator's shell sees of an SGI board engine: the `BoardEngine`
//! trait, its types, a bounded event queue and a C ABI (`ffi`) that every
//! board shares. A board crate implements the trait and its own constructor;
//! it depends on this crate and not the other way round.
//!
//! The shell owns buses, interrupt routing, clocks, locking policy and
//! persistence. The engine owns everything from the board's command stream
//! down. The rules every board keeps:
//!
//! **Calls.** Every call comes with the BQL held (or, in another shell, from
//! one thread at a time), and different calls may come from different threads
//! in turn, so an engine is `Send`. A call never re-enters: the engine calls
//! back only into `HostMem` and the notify hook, and those must not call any
//! engine function. The C ABI detects a re-entering (or concurrent) call and
//! answers it as a fault without touching the engine; the shell should also
//! guard its own MMIO region against re-entrant DMA. The notify hook may be
//! called from any thread, with engine locks held, and may only schedule the
//! shell's pump; it is never called once the engine is dropped.
//!
//! **Never waits on the shell.** No call waits for an input only the shell
//! can give (a `frame`, a time). An engine whose stream holds for one says so:
//! `wait_idle` and `advance` answer `Idle::Stalled`, and `quiesce` answers
//! `Midway`. The shell's loop (`shell::drain_stalls`) gives the input and
//! asks again. A read that answers `Busy` on a bus that cannot retry runs
//! that loop with no bound: nothing the worker needs needs the BQL, and a
//! stall is cleared by the loop itself.
//!
//! **Faults.** After a device fault every call that can answer `Fault` does
//! (`write`, `read`, `Idle::Faulted`, `Refused::Faulted`, `observe`), `push_words`
//! and `run` do nothing, `access_waits` answers false; `drain_events`,
//! `fault`, `irq_levels`, `quiesce` (which still parks) and `reset` keep working.
//! A panic in an engine thread or a call becomes a fault the same way.
//!
//! **Quiescence.** `quiesce` before `observe_all`, `save`, `load` and `reset`,
//! and when the VM stops (the shell does it from its run-state handler, and
//! `resume`s when it runs again): a worker still draining keeps writing guest
//! RAM and lent memory, and migration's final RAM pass runs before devices
//! save. Read `drain_dirty` after the quiesce.
//!
//! **Pump order.** An event that follows from a write-back (a fence's
//! interrupt) is posted only after the write-back is complete and visible:
//! by `advance`, or by the worker that did the write once its `ram_write`
//! has returned. The guest cannot take the interrupt and read a stale fence.
//! A worker that retires stores (and so owes credits) calls `Outbox::wake`,
//! so the shell pumps to return them.
//!
//! **Conformance.** The clauses that do not depend on a board's registers are
//! `conformance` (feature `conformance`): every board's tests implement its
//! `Subject` and run them.
//!
//! **Time.** Passed in on `advance`, `frame` and `wait_idle`; the engine reads
//! no host clock but for the host-time `budget_ns` the shell gives.

#[cfg(feature = "conformance")]
pub mod conformance;
pub mod engine;
pub mod ffi;
pub mod gpu_loss;
pub mod inject;
#[cfg(feature = "conformance")]
pub mod fuzz;
pub mod mem;
#[cfg(feature = "mock")]
pub mod mock;
pub mod outbox;
pub mod shell;
pub mod state;

pub use engine::*;
pub use mem::{DirtyBlocks, DirtyRun, HostMem, Lent, MemFault, Swap, DIRTY_BLOCK};
pub use gpu_loss::GpuLoss;
pub use outbox::Outbox;
pub use state::{Chunk, StateSink, StateSource, VecSource};
