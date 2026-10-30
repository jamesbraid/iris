//! Ported from hw/display/sgi/head-work.c, head-work.h, head-guard.c and
//! head-guard.h in jamesbraid/qemu-sgi-origami.
//!
//! SGI graphics head work that outlasts the store that started it. A head
//! that runs a long command stream inside the processor store that started
//! it keeps that processor from its interrupts for as long as the stream
//! takes. This lets a family run such work in slices instead: a little
//! inside the store, the rest on a thread of the head's own. The family
//! decides what the work is, which accesses must wait for it, and what waits
//! behind it.
//!
//! QEMU's big lock is the owner's state lock here. The family hands
//! `sgi_head_work_start` an [`SGIHeadWorkBql`] that takes and lets go of the
//! very mutex its register accesses run under, and the head's thread holds
//! it exactly where the C holds the big lock: all the time, except while it
//! waits for a kick and while a draw has let go with `sgi_head_work_unlock`.
//! `qemu_cond_wait_bql` becomes [`sgi_head_work_cond_wait`], which takes a
//! small mutex of its own before letting go of the big lock, so a signal
//! sent under the big lock cannot fall between a waiter's check and its
//! sleep.
//!
//! The QEMUTimer becomes a deadline (`timer`, on the virtual clock, 0 when
//! not armed) that the head's thread sleeps towards; reaching it does what
//! the C's timer callback does. `qemu_clock_virtual_ns` is the host's
//! monotonic clock in iris, so the clock does not stop with the machine;
//! the owner drains the work and stops the thread when the machine stops
//! instead, as the C's run-state handler drains it.
//!
//! Until `sgi_head_work_start` has run (a state built for a unit test, say)
//! there is no thread, and `sgi_head_work_schedule` runs the pending work on
//! the caller so nothing is stranded.
//!
//! Split locking (`sgi_head_work_init_split`) is an iris addition. Letting
//! go of the owner's lock part-way through a draw is sound in C under its
//! own rules, but in Rust the draw's frames still hold `&mut` references
//! into the state while another thread takes the lock and makes its own.
//! Under split locking the work's own fields sit under one lock (`bql`) and
//! the state it works on under another, which the head's thread holds for a
//! whole step without letting go; whatever another thread must reach while
//! a step runs belongs under `bql`.
//!
//! Fields both the head's thread and another thread reach (`slice`,
//! `drawing`, `go`, `exit`, `timer`) are only touched under the big lock,
//! as in the C. Loops that wait for another thread to change one read it
//! through a raw pointer the wait itself receives, so the compiler rereads
//! it after each wait.
use super::*;

use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

/// How much one slice may do: host time, because a slice exists to bound
/// how long its thread keeps the device, and for qtest a count of words.
/// A slice checks the budget between words, so it always does at least one
/// and stops at the first word boundary past the deadline.
#[derive(Clone, Copy, Default, Debug)]
pub struct SGIHeadWorkBudget {
    pub deadline: i64, // QEMU_CLOCK_REALTIME; INT64_MAX: none
    pub words: u32,    // UINT32_MAX: none
}

/// One slice of host time. The guest's scheduler clock ticks every 10 ms,
/// so a slice stays well below that.
pub const SGI_HEAD_WORK_SLICE_NS: i64 = 1 * SCALE_MS;

#[derive(Clone, Copy, Default)]
pub struct SGIHeadWorkOps {
    /// Go on with the work until the budget is spent or nothing is left.
    pub step: Option<fn(Opaque, &mut SGIHeadWorkBudget)>,
    /// Whether any work is left.
    pub pending: Option<fn(Opaque) -> bool>,
    /// After a slice on the head's thread, under the big lock.
    pub slice_done: Option<fn(Opaque)>,
}

/// The owner's big lock: the mutex every access to the head's state runs
/// under. `lock` blocks until this thread holds it; `unlock` lets go of a
/// hold this thread has, whether `lock` took it or a register access's
/// caller holds its guard.
pub trait SGIHeadWorkBql: Send + Sync {
    fn lock(&self);
    /// # Safety
    /// The calling thread holds the lock, and takes it back with `lock`
    /// before it touches anything the lock covers.
    unsafe fn unlock(&self);
}

/// The condition variables, and the thread's handle.
#[derive(Default)]
pub struct SGIHeadWorkSync {
    /// Taken by a waiter before it lets go of the big lock, and by a
    /// signaller under the big lock (`sgi_head_work_cond_wait`).
    pub m: StdMutex<()>,
    pub kick: Condvar,
    pub done: Condvar,
    pub handle: StdMutex<Option<JoinHandle<()>>>,
}

#[derive(Clone, Default)]
pub struct SGIHeadWork {
    pub ops: P<SGIHeadWorkOps>,
    pub opaque: Opaque,
    /// The name the C gives the thread at init.
    pub name: String,
    /// Host-only: words a slice for qtest; 0 means host time.
    pub test_words: u32,
    /// When the work last became pending, on the virtual clock; 0: idle.
    pub pending_since: i64,
    /// The C's QEMUTimer: when the next slice is due (virtual clock); 0:
    /// not armed.
    pub timer: i64,
    pub sync: Option<Arc<SGIHeadWorkSync>>,
    pub bql: Option<Arc<dyn SGIHeadWorkBql>>,
    /// Split locking (`sgi_head_work_init_split`): the work's fields live
    /// under `bql`, and the state `step` works on under this lock, which
    /// the head's thread holds for a whole step and nothing else.
    pub state_lock: Option<Arc<dyn SGIHeadWorkBql>>,
    pub split: bool,
    /// What `pending` receives: `opaque`, or under split locking the
    /// state `bql` covers.
    pub pending_opaque: Opaque,
    /// The head's thread, while it runs.
    pub thread: Option<ThreadId>,
    pub started: bool,
    pub go: bool,
    pub exit: bool,
    /// A slice runs on the head's thread.
    pub slice: bool,
    /// ... and has let go of the big lock.
    pub drawing: bool,
}

/// The budget for a slice; `timed` bounds it by host time.
pub fn sgi_head_work_budget(w: &mut SGIHeadWork, timed: bool) -> SGIHeadWorkBudget {
    SGIHeadWorkBudget {
        deadline: if timed { qemu_clock_realtime_ns() + SGI_HEAD_WORK_SLICE_NS } else { i64::MAX },
        words: if w.test_words != 0 { w.test_words } else { u32::MAX },
    }
}

/// Count one word against the budget; true when the slice must stop.
pub fn sgi_head_work_spent(b: &mut SGIHeadWorkBudget) -> bool {
    if b.words != u32::MAX {
        b.words = b.words.wrapping_sub(1);
        if b.words == 0 {
            return true;
        }
    }
    b.deadline != i64::MAX && qemu_clock_realtime_ns() >= b.deadline
}

/// `qemu_cond_wait_bql`: let go of the big lock, sleep on the head's `done`
/// (or `kick`) condition until signalled or `timeout_ns` passes (when it is
/// not negative), and take the big lock back. The caller holds the big
/// lock. `w` is a raw pointer so the caller's loop rereads what it waits on.
pub fn sgi_head_work_cond_wait(w: *mut SGIHeadWork, done: bool, timeout_ns: i64) {
    let (sync, bql) = unsafe { ((*w).sync.clone(), (*w).bql.clone()) };
    let (Some(sync), Some(bql)) = (sync, bql) else {
        // No head thread, so nothing could signal.
        return;
    };
    let cond = if done { &sync.done } else { &sync.kick };
    let g = sync.m.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the caller holds the big lock, and touches nothing it covers
    // until bql.lock() below returns.
    unsafe { bql.unlock() };
    let g = if timeout_ns >= 0 {
        cond.wait_timeout(g, Duration::from_nanos(timeout_ns as u64)).unwrap_or_else(|e| e.into_inner()).0
    } else {
        cond.wait(g).unwrap_or_else(|e| e.into_inner())
    };
    drop(g);
    bql.lock();
}

/// `qemu_cond_signal` / `qemu_cond_broadcast`, under the big lock.
pub fn sgi_head_work_cond_signal(w: &SGIHeadWork, done: bool) {
    if let Some(sync) = &w.sync {
        let _g = sync.m.lock().unwrap_or_else(|e| e.into_inner());
        if done {
            sync.done.notify_all();
        } else {
            sync.kick.notify_all();
        }
    }
}

/// The next slice runs a short gap later on the machine clock. The gap is
/// for the processors: outside its draws a slice holds the big lock, and a
/// thread that drops the lock and takes it straight back usually wins it
/// again.
pub const SGI_HEAD_WORK_GAP_NS: i64 = 100 * SCALE_US;

/// Work has been left pending: run the next slice soon.
pub fn sgi_head_work_schedule(w: &mut SGIHeadWork) {
    if w.pending_since == 0 {
        w.pending_since = qemu_clock_virtual_ns().max(1);
    }
    if !w.started {
        if w.split {
            // The caller holds bql, not the state lock; it drains.
            return;
        }
        // No head thread: finish the work on the caller.
        sgi_head_work_enter_work();
        sgi_head_work_drain(w);
        sgi_head_work_leave();
        return;
    }
    if w.timer == 0 {
        w.timer = qemu_clock_virtual_ns() + SGI_HEAD_WORK_GAP_NS;
        // The thread sleeps towards the deadline; wake it to see a new one.
        sgi_head_work_cond_signal(w, false);
    }
}

/// Forget the work's timing once the family has abandoned it.
pub fn sgi_head_work_idle(w: &mut SGIHeadWork) {
    w.pending_since = 0;
    w.timer = 0;
}

/// Whether work has been pending for longer than `ns` of machine time.
pub fn sgi_head_work_overdue(w: &mut SGIHeadWork, ns: i64) -> bool {
    w.pending_since != 0 && qemu_clock_virtual_ns() - w.pending_since > ns
}

/// Wait out a slice that has let go of the big lock on another thread.
pub fn sgi_head_work_wait_slice(w: &mut SGIHeadWork) {
    let w: *mut SGIHeadWork = w;
    while unsafe { std::ptr::read_volatile(&(*w).slice) } {
        sgi_head_work_cond_wait(w, true, -1);
    }
}

/// Finish all the work now, on this thread.
pub fn sgi_head_work_drain(w: &mut SGIHeadWork) {
    let mut budget = SGIHeadWorkBudget { deadline: i64::MAX, words: u32::MAX };

    sgi_head_work_wait_slice(w);
    let ops = *w.ops.get();
    let opaque = w.opaque;
    if ops.pending.expect("pending hook")(opaque) {
        ops.step.expect("step hook")(opaque, &mut budget);
    }
    sgi_head_work_idle(w);
}

/// Whether a draw on this thread may let go of the big lock: inside a slice
/// on the head's own thread, which every other thread that decodes the
/// work waits out. A family brackets such a draw with
/// `sgi_head_work_unlock` and `sgi_head_work_relock`, and must arrange that
/// nothing else touches what the draw reads or writes meanwhile.
///
/// The family reaches its state through the opaque raw pointer there
/// (`from_opaque`), not through a `&mut` function parameter, so the
/// compiler does not assume that nothing else writes the state while the
/// lock is let go.
///
/// Always false here: QEMU lets the head's thread release the big lock
/// around a long draw, but in Rust the processor thread would then take a
/// second `&mut` to the board state while the draw still holds one. Draws
/// keep the board lock for their whole length instead, so
/// `sgi_head_work_unlock` and `sgi_head_work_relock` are never reached.
/// Split locking (`sgi_head_work_init_split`) lets other threads reach
/// what they need during a draw without it.
pub fn sgi_head_work_may_unlock(_w: &mut SGIHeadWork) -> bool {
    false
}

pub fn sgi_head_work_unlock(w: &mut SGIHeadWork) {
    let w: *mut SGIHeadWork = w;
    unsafe {
        (*w).drawing = true;
        let bql = (*w).bql.clone().expect("big lock");
        // SAFETY: the head's thread holds the big lock during a slice
        // (may_unlock), and relock takes it back.
        bql.unlock();
    }
}

pub fn sgi_head_work_relock(w: &mut SGIHeadWork) {
    let w: *mut SGIHeadWork = w;
    unsafe {
        let bql = (*w).bql.clone().expect("big lock");
        bql.lock();
        std::ptr::write_volatile(&mut (*w).drawing, false);
    }
}

/// The C's timer callback: the next slice is due.
pub fn sgi_head_work_timer(w: &mut SGIHeadWork) {
    w.go = true;
    sgi_head_work_cond_signal(w, false);
}

/// A slice runs on the head's own thread: the thread that raises the
/// processors' timer interrupts must not be held for as long as a draw
/// takes.
pub fn sgi_head_work_thread(w: *mut SGIHeadWork) {
    let bql = unsafe { (*w).bql.clone().expect("big lock") };

    bql.lock();
    loop {
        // Wait for a kick, firing the timer once its deadline passes.
        loop {
            let (go, exit, timer) = unsafe {
                (std::ptr::read_volatile(&(*w).go), std::ptr::read_volatile(&(*w).exit), std::ptr::read_volatile(&(*w).timer))
            };
            if go || exit {
                break;
            }
            if timer == 0 {
                sgi_head_work_cond_wait(w, false, -1);
                continue;
            }
            let now = qemu_clock_virtual_ns();
            if now >= timer {
                let wr = unsafe { &mut *w };
                wr.timer = 0;
                sgi_head_work_timer(wr);
            } else {
                sgi_head_work_cond_wait(w, false, timer - now);
            }
        }
        let wr = unsafe { &mut *w };
        if wr.exit {
            break;
        }
        wr.go = false;
        let mut budget = sgi_head_work_budget(wr, true);
        let ops = *wr.ops.get();
        let opaque = wr.opaque;
        let pending_opaque = wr.pending_opaque;
        let state_lock = wr.state_lock.clone();
        sgi_head_work_enter_work();
        wr.slice = true;
        if let Some(state_lock) = state_lock {
            // Split locking: the step runs under the state lock alone, so
            // what bql covers stays open to other threads meanwhile. No
            // reference into either lock's data lives across the switch.
            // SAFETY: held since the wait above; taken back below.
            unsafe { bql.unlock() };
            state_lock.lock();
            ops.step.expect("step hook")(opaque, &mut budget);
            // SAFETY: taken just above.
            unsafe { state_lock.unlock() };
            bql.lock();
        } else {
            ops.step.expect("step hook")(opaque, &mut budget);
        }
        let wr = unsafe { &mut *w };
        wr.slice = false;
        sgi_head_work_leave();
        sgi_head_work_cond_signal(wr, true);
        if ops.pending.expect("pending hook")(pending_opaque) {
            sgi_head_work_schedule(unsafe { &mut *w });
        } else {
            sgi_head_work_idle(unsafe { &mut *w });
        }
        if let Some(slice_done) = ops.slice_done {
            slice_done(opaque);
        }
    }
    // SAFETY: taken before the loop, or back by the last wait.
    unsafe { bql.unlock() };
}

/// The C creates the head's thread here; iris starts it separately
/// (`sgi_head_work_start`), once the owner's state sits behind its lock.
pub fn sgi_head_work_init(w: &mut SGIHeadWork, name: &str, ops: &SGIHeadWorkOps, opaque: Opaque) {
    w.ops = P::from_ref(ops);
    w.opaque = opaque;
    w.pending_opaque = opaque;
    w.name = name.to_string();
    w.sync = Some(Arc::new(SGIHeadWorkSync::default()));
}

/// Split locking: `step(opaque)` runs on the head's thread under the state
/// lock `sgi_head_work_start_split` names, and nothing else does there;
/// `pending(pending_opaque)` and the work's own fields are under the lock
/// it names as `bql`. Without a thread, `sgi_head_work_schedule` leaves the
/// work to the caller.
pub fn sgi_head_work_init_split(w: &mut SGIHeadWork, name: &str, ops: &SGIHeadWorkOps, opaque: Opaque, pending_opaque: Opaque) {
    sgi_head_work_init(w, name, ops, opaque);
    w.pending_opaque = pending_opaque;
    w.split = true;
}

/// Start a split-locked head's thread. The caller holds `bql` and not
/// `state_lock`.
pub fn sgi_head_work_start_split(w: &mut SGIHeadWork, thread_name: &str, bql: Arc<dyn SGIHeadWorkBql>, state_lock: Arc<dyn SGIHeadWorkBql>) {
    assert!(w.split);
    w.state_lock = Some(state_lock);
    sgi_head_work_start(w, thread_name, bql);
}

/// Start the head's thread, named `thread_name`. The caller holds `bql`,
/// the lock every access to the head's state runs under, and `w` stays at
/// its address until `sgi_head_work_cleanup` has run.
pub fn sgi_head_work_start(w: &mut SGIHeadWork, thread_name: &str, bql: Arc<dyn SGIHeadWorkBql>) {
    if w.started {
        return;
    }
    let sync = w.sync.get_or_insert_with(|| Arc::new(SGIHeadWorkSync::default())).clone();
    w.bql = Some(bql);
    w.go = false;
    w.exit = false;
    w.slice = false;
    w.drawing = false;
    let p = P(w as *mut SGIHeadWork);
    let handle = std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || {
            let p = p;
            sgi_head_work_thread(p.0)
        })
        .expect("head work thread");
    w.thread = Some(handle.thread().id());
    *sync.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    w.started = true;
}

/// Stops the thread; the family abandons any work left afterwards. The
/// caller holds the big lock.
pub fn sgi_head_work_cleanup(w: &mut SGIHeadWork) {
    if !w.started {
        return;
    }
    let p: *mut SGIHeadWork = w;
    let (sync, bql) = unsafe { ((*p).sync.clone().expect("sync"), (*p).bql.clone().expect("big lock")) };
    // The thread needs the big lock to see that it should stop.
    unsafe {
        (*p).exit = true;
        sgi_head_work_cond_signal(&*p, false);
        // SAFETY: the caller holds the big lock; it is taken back below.
        bql.unlock();
    }
    if let Some(handle) = sync.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = handle.join();
    }
    bql.lock();
    let w = unsafe { &mut *p };
    w.timer = 0;
    w.thread = None;
    // The lock may own the state that owns this work: let go of it.
    w.bql = None;
    w.state_lock = None;
    w.started = false;
}

// head-guard.c: the per-thread re-entry guard.
//
// Whether this thread is inside an SGI head's register access or work. A
// head's DMA can be aimed at another head's registers, and that head's
// access can let go of the big lock to wait for its own work, so an access
// that arrives from inside any head's access or work is refused. Bracket
// every register access and every slice with enter and leave.

/// Whether this thread may enter a head's register access; false for an
/// access that must be refused.
pub fn sgi_head_work_enter() -> bool {
    SGI_HEAD_WORK_DEPTH.with(|d| {
        if d.get() != 0 {
            return false;
        }
        d.set(d.get() + 1);
        true
    })
}

pub fn sgi_head_work_leave() {
    SGI_HEAD_WORK_DEPTH.with(|d| {
        assert!(d.get() != 0);
        d.set(d.get() - 1);
    })
}

/// Bracket a slice or drain that runs outside any register access.
pub fn sgi_head_work_enter_work() {
    SGI_HEAD_WORK_DEPTH.with(|d| d.set(d.get() + 1))
}

thread_local! {
    /// Register accesses and slices this thread is inside, across all heads.
    pub static SGI_HEAD_WORK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}
