//! Idle parking: let a processor's host thread sleep while the guest spins
//! in a loop that cannot get anywhere until something outside it changes.
//!
//! A guest idles by spinning: IRIX on the R10000 family polls its run
//! queue in a kernel loop and never executes WAIT. Running that loop costs
//! a host core per emulated processor. Parking stops running it, and is
//! exact when the loop provably could not have left by itself:
//!
//! 1. Candidate. After each run-loop batch the PC is recorded in a small
//!    ring ([`IdleParkState`]). One long kernel PC cluster dominating the
//!    ring, with interrupts enabled and no line pending, makes the loop a
//!    candidate. This is only a filter; it decides nothing.
//! 2. Proof. The executor runs the candidate once more under observation
//!    (`MipsExecutor::idle_probe_run`), one interpreted instruction at a
//!    time, decoding each load and store before it runs ([`access`]). It
//!    requires that the whole state the loop can depend on returns
//!    to itself: the registers ([`State`]) and the contents of every line
//!    the loop touches, its code included. It refuses an uncached access,
//!    an exception, a pending interrupt line and any instruction that
//!    reaches outside that state ([`reaches_outside`]). The loop may store,
//!    as IRIX does to its stack, as long as memory comes back with the
//!    registers.
//! 3. Park. The loop's next state is a function of that state, the watched
//!    lines and the interrupt lines, and the state is in a cycle. While the
//!    lines and the interrupt word stay as they are the loop would run that
//!    cycle for ever, so not running it is the same as running it slowly;
//!    iris gives a processor no fixed speed. The park stops just after the
//!    store that starts the cycle's longest store-free stretch, so a word
//!    the loop sets and clears is left cleared.
//! 4. Wake. Any interrupt line ends the park at once through [`wake`]: the
//!    sleeper announces itself in [`PARKED`] before its last look at the
//!    pending word, and every writer sets its bit before reading
//!    [`PARKED`], so one of them sees the other. A store by another
//!    processor or a device reaches memory without passing anything a
//!    sleeper can wait on, so the watched lines are compared every
//!    [`poll_interval`]. The snapshot is taken at the park point and checked
//!    against the verified pass, so a write that lands before the park is
//!    seen too. A memory-only wakeup is late by at most one poll, which is
//!    the loop running slowly again.
//!
//! The instruction loops carry none of this: the PC is recorded after the
//! batch, and the probe steps the interpreter itself.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::cpu::mips_core::{MipsCore, PrivilegeMode, CAUSE_IP_MASK, STATUS_IM_MASK};
use crate::cpu::smp::MAX_CPUS;

/// PCs kept for the candidate test, one per run-loop batch (1,000
/// instructions, 10,000 in a `lightning` build): many passes of even a long
/// idle loop. A loop whose period divides the batch length lands on few
/// distinct PCs and is not offered, which errs towards running.
const PC_RING: usize = 32;
/// A cluster is a run of sorted sample PCs no more than this far apart.
/// IRIX's idle path is one loop plus a few helpers reached by calls.
const CLUSTER_GAP_BYTES: u64 = 256;
/// The dominant cluster must hold this many of the samples ...
const CLUSTER_MIN_SAMPLES: usize = 12;
/// ... at this many distinct PCs, which leaves 2-3 instruction delays and
/// device polls out before any probe is spent on them.
const CLUSTER_MIN_DISTINCT: usize = 12;
/// Offer nothing in the first minute of CPU time: boot is busy, and every
/// refused probe is paid in full. `IRIS_IDLE_WARMUP_SECS` overrides it.
const PARK_WARMUP: Duration = Duration::from_secs(60);
/// One guest cycle per 10 ns of park, the host-pacing rate `ci_clock`
/// names: cross-thread readers of `hot.cycles` (WD33C93A's deferred
/// interrupt, CP0 Random) keep seeing progress.
const STALL_NS_PER_CYCLE: u64 = 10;

/// Most lines one proof watches. IRIX's idle loop touches 15-47.
pub const MAX_LINES: usize = 64;
/// Watch granularity, bytes.
pub const LINE: u64 = 64;
/// Most distinct PCs a probe tracks.
const MAX_PCS: usize = 512;
/// Most anchor visits compared for a repeat.
pub const MAX_STATES: usize = 64;
/// Step budget of one probe. IRIX's idle loop is about 70 instructions a
/// pass; a loop that has not closed its cycle by now is not one.
pub const MAX_STEPS: usize = 4_096;
/// Most steps between two visits of the probe's starting PC. A probe runs
/// the guest at a fraction of its speed, so when the loop leaves to do real
/// work (another processor handed it some) the probe must give up at once
/// rather than run that work slowly to the end of its budget.
pub const MAX_PASS: usize = 512;

/// The CPU thread of each parked processor, so [`wake`] can unpark it.
static PARKER: parking_lot::Mutex<[Option<std::thread::Thread>; MAX_CPUS]> = parking_lot::const_mutex([const { None }; MAX_CPUS]);
/// A bit per processor, set from just before its last look at the pending
/// word until it runs again. A rule that keeps processors in step should
/// read a set bit as idle.
pub static PARKED: AtomicU8 = AtomicU8::new(0);
/// One machine per process; the warm-up must not restart when the monitor
/// stops and starts a CPU thread.
static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Counters for the monitor's `idle status`.
pub static PARK_COUNT: AtomicU64 = AtomicU64::new(0);
pub static PARK_SLEEP_NS: AtomicU64 = AtomicU64::new(0);
pub static PARK_WAKE_IRQ: AtomicU64 = AtomicU64::new(0);
pub static PARK_WAKE_MEM: AtomicU64 = AtomicU64::new(0);
pub static PARK_WAKE_OTHER: AtomicU64 = AtomicU64::new(0);
pub static PROBES: AtomicU64 = AtomicU64::new(0);
/// Host time spent in probes: a probe runs the guest at a fraction of its
/// speed, so this is what proving costs.
pub static PROBE_NS: AtomicU64 = AtomicU64::new(0);
pub static REFUSED: [AtomicU64; Refusal::COUNT] = [const { AtomicU64::new(0) }; Refusal::COUNT];

/// Wake the parked processors. Call after setting a bit in `hot.interrupts`
/// from any thread; one atomic load when nothing is parked. Each wakes and
/// looks at its own word; one with nothing pending sleeps again.
#[inline]
pub fn wake() {
    if PARKED.load(Ordering::SeqCst) != 0 {
        for t in PARKER.lock().iter().flatten() {
            t.unpark();
        }
    }
}

/// Make this thread the one [`wake`] unparks for `core`. Once per park.
pub(crate) fn register_parker(core: &MipsCore) {
    PARKER.lock()[core.cpu_id as usize] = Some(std::thread::current());
}

/// Announce `core` parked. Call before the last look at the pending word
/// (see the module doc).
pub(crate) fn announce_parked(core: &MipsCore) {
    PARKED.fetch_or(1 << core.cpu_id, Ordering::SeqCst);
}

pub(crate) fn clear_parked(core: &MipsCore) {
    PARKED.fetch_and(!(1 << core.cpu_id), Ordering::SeqCst);
}

/// Move `hot.cycles` on by a park's host time and record that those cycles
/// were not instructions (the status bars' MIPS leaves them out).
pub(crate) fn advance_parked(core: &mut MipsCore, elapsed_ns: u64) {
    let cycles = elapsed_ns / STALL_NS_PER_CYCLE;
    core.hot.cycles = core.hot.cycles.wrapping_add(cycles);
    core.hot.parked_cycles = core.hot.parked_cycles.wrapping_add(cycles);
}

/// How often a parked processor compares the lines its loop watches.
/// `IRIS_IDLE_POLL_US` overrides it.
pub fn poll_interval() -> Duration {
    let us = std::env::var("IRIS_IDLE_POLL_US").ok().and_then(|s| s.parse().ok()).unwrap_or(200u64);
    Duration::from_micros(us.max(10))
}

/// Offers a processor's loop for a probe when it looks idle, and backs off
/// after refusals.
pub struct IdleParkState {
    pcs: [u64; PC_RING],
    user: [bool; PC_RING],
    len: usize,
    pos: usize,
    /// Offer nothing before `hot.cycles` reaches this.
    skip_until_cycle: u64,
    /// Probes refused in a row.
    refusals: u32,
    started: Instant,
    warmup: Duration,
}

impl Default for IdleParkState {
    fn default() -> Self {
        let warmup = std::env::var("IRIS_IDLE_WARMUP_SECS").ok().and_then(|s| s.parse().ok()).map_or(PARK_WARMUP, Duration::from_secs);
        Self {
            pcs: [0; PC_RING],
            user: [false; PC_RING],
            len: 0,
            pos: 0,
            skip_until_cycle: 0,
            refusals: 0,
            started: *PROCESS_START.get_or_init(Instant::now),
            warmup,
        }
    }
}

impl IdleParkState {
    /// With the warm-up already over.
    #[cfg(test)]
    pub(crate) fn ready() -> Self {
        Self { warmup: Duration::ZERO, ..Self::default() }
    }

    /// Record the PC after a batch.
    #[inline]
    pub fn record(&mut self, core: &MipsCore) {
        self.pcs[self.pos] = core.pc;
        self.user[self.pos] = core.get_privilege_mode() != PrivilegeMode::Kernel;
        self.pos = (self.pos + 1) % PC_RING;
        self.len = (self.len + 1).min(PC_RING);
    }

    /// Should the run loop probe now? Yes when interrupts are enabled, no
    /// line is pending, the processor is in kernel mode, and either the ring
    /// shows one long kernel loop or `again` says the PC is back in the code
    /// of the last proven spin (so the ring need not refill after every
    /// interrupt).
    pub fn should_probe(&mut self, core: &MipsCore, again: bool) -> bool {
        if self.started.elapsed() < self.warmup || core.hot.cycles < self.skip_until_cycle {
            return false;
        }
        let pending = (core.cp0_cause | core.hot.interrupts.load(Ordering::Relaxed) as u32) & CAUSE_IP_MASK;
        if pending != 0 || !core.interrupts_enabled() || core.cp0_status & STATUS_IM_MASK == 0 {
            self.len = 0;
            return false;
        }
        core.get_privilege_mode() == PrivilegeMode::Kernel && (again || self.window_is_idle())
    }

    /// One long kernel cluster dominates a full ring, user samples at most a
    /// quarter of it.
    fn window_is_idle(&self) -> bool {
        if self.len < PC_RING {
            return false;
        }
        let users = self.user.iter().filter(|&&u| u).count();
        let mut kernel: Vec<u64> = (0..PC_RING).filter(|&i| !self.user[i]).map(|i| self.pcs[i]).collect();
        kernel.sort_unstable();
        let (samples, distinct) = kernel
            .chunk_by(|a, b| b - a <= CLUSTER_GAP_BYTES)
            .map(|c| (c.len(), 1 + c.windows(2).filter(|w| w[0] != w[1]).count()))
            .max()
            .unwrap_or((0, 0));
        users <= PC_RING / 4 && samples >= CLUSTER_MIN_SAMPLES && distinct >= CLUSTER_MIN_DISTINCT
    }

    /// A probe refused: offer nothing for 64 batches, doubling for each
    /// refusal in a row up to 65,536, so a busy loop that keeps looking idle
    /// costs a bounded share of the processor.
    pub fn refused(&mut self, core: &MipsCore) {
        let batches = 64u64 << self.refusals.min(10);
        self.refusals = self.refusals.saturating_add(1);
        self.skip_until_cycle = core.hot.cycles.saturating_add(batches * 1000);
    }

    pub fn proven(&mut self) {
        self.refusals = 0;
    }
}

/// Why a probe would not call a loop a spin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The whole state never repeated within the step budget.
    NoCycle,
    /// An uncached access: the loop reads or writes a device.
    Uncached,
    /// The loop took an exception.
    Exception,
    /// An interrupt line became pending.
    Interrupt,
    /// An instruction that reaches outside the compared state.
    Outside,
    /// Memory changed under the probe.
    Changed,
    /// A breakpoint or a stop request.
    Stopped,
    /// More lines than a proof watches.
    TooManyLines,
    /// An access the probe does not follow.
    Unfollowed,
}

impl Refusal {
    pub const COUNT: usize = 9;
    pub const NAMES: [&'static str; Self::COUNT] =
        ["no_cycle", "uncached", "exception", "interrupt", "outside", "changed", "stopped", "too_many_lines", "unfollowed"];
}

/// What one probe saw.
#[derive(Default)]
pub struct Probe {
    pub pcs: Vec<u64>,
    /// Lines read, stored to or executed, in first-touch order.
    pub lines: Vec<u64>,
    /// Stores that changed memory; a store of what was already there does
    /// not count.
    pub stores: u32,
    /// Lines stored to in the current step, with what they held before.
    pub pending: Vec<(u64, [u64; 8])>,
    pub uncached: bool,
    pub too_many_lines: bool,
}

impl Probe {
    pub fn note_pc(&mut self, pc: u64) {
        if self.pcs.len() < MAX_PCS && !self.pcs.contains(&pc) {
            self.pcs.push(pc);
        }
    }


    /// A store to `line`, which held `before`; settled after the step.
    pub fn note_store(&mut self, line: u64, before: [u64; 8]) {
        self.add_line(line);
        self.pending.push((line, before));
    }

    /// After a step: count the stores that changed their line.
    pub fn settle_stores(&mut self, mut read: impl FnMut(u64) -> Option<[u64; 8]>) {
        for (line, before) in std::mem::take(&mut self.pending) {
            if read(line) != Some(before) {
                self.stores += 1;
            }
        }
    }

    pub fn add_line(&mut self, line: u64) {
        if !self.lines.contains(&line) {
            if self.lines.len() < MAX_LINES {
                self.lines.push(line);
            } else {
                self.too_many_lines = true;
            }
        }
    }

    /// The accesses alone rule the loop out.
    pub fn refusal(&self) -> Option<Refusal> {
        if self.uncached {
            Some(Refusal::Uncached)
        } else if self.too_many_lines {
            Some(Refusal::TooManyLines)
        } else {
            None
        }
    }
}

/// The memory an instruction will access, from its encoding and the
/// registers it is about to run with (MIPS IV): `Ok(None)` for none,
/// `Ok(Some((store, address)))`, `Err` for an encoding a proof does not
/// follow. Prefetches touch nothing a loop can observe.
pub fn access(w: u32, gpr: &[u64; 32]) -> Result<Option<(bool, u64)>, ()> {
    let base = gpr[((w >> 21) & 0x1f) as usize];
    let at = base.wrapping_add(w as u16 as i16 as i64 as u64);
    Ok(match w >> 26 {
        0x1a | 0x1b | 0x20..=0x27 | 0x30..=0x32 | 0x34..=0x37 => Some((false, at)),
        0x28..=0x2e | 0x39 | 0x3a | 0x3d..=0x3f => Some((true, at)),
        // COP1X indexed loads and stores, base + index.
        0x13 if w & 0x3f < 0x10 => {
            let at = base.wrapping_add(gpr[((w >> 16) & 0x1f) as usize]);
            match w & 0x3f {
                0x0f => None,
                f => Some((f >= 8, at)),
            }
        }
        0x13 => None,
        // PREF; CACHE and SC/SCD are refused by `reaches_outside`.
        0x33 => None,
        0x2f | 0x38 | 0x3c => return Err(()),
        _ => None,
    })
}

/// Does the instruction reach outside the state a proof compares: an effect
/// the state does not show, a store the hooks cannot see, or a read of
/// something that changes by itself? MIPS IV encodings.
pub fn reaches_outside(w: u32) -> bool {
    match w >> 26 {
        // SYSCALL, BREAK and the register traps.
        0x00 => matches!(w & 0x3f, 0x0c | 0x0d | 0x30..=0x34 | 0x36),
        // The immediate traps.
        0x01 => matches!((w >> 16) & 0x1f, 0x08..=0x0c | 0x0e),
        // COP0: every CO operation (TLB operations, ERET, WAIT); a move to
        // anything but Status; a read of Random or Count, which change by
        // themselves (a loop can repeat its state now and still leave by
        // time alone).
        0x10 => {
            let (rs, rd) = ((w >> 21) & 0x1f, (w >> 11) & 0x1f);
            rs & 0x10 != 0
                || match rs {
                    0x00 | 0x01 => rd == 1 || rd == 9,
                    0x04 | 0x05 => rd != 12,
                    _ => false,
                }
        }
        // CACHE.
        0x2f => true,
        // SC and SCD: with several processors a successful SC is a host
        // compare-exchange that bypasses the store hooks.
        0x38 | 0x3c => true,
        _ => false,
    }
}

/// The registers a proof compares: everything the loop can read except
/// k0/k1, the kernel's exception scratch, which an interrupt between
/// passes leaves junk in and the loop never reads.
#[derive(Clone, PartialEq, Eq)]
pub struct State(Vec<u64>);

impl State {
    pub fn of(core: &MipsCore, llbit: bool) -> Self {
        let mut w = vec![core.pc, core.in_delay_slot as u64, core.delay_slot_target];
        w.extend(core.gpr.iter().enumerate().filter(|&(i, _)| i != 26 && i != 27).map(|(_, &r)| r));
        w.extend([core.hi, core.lo]);
        w.extend_from_slice(&core.fpr);
        w.extend([core.fpu_fcsr as u64, core.cp0_status as u64, core.ll_value, core.ll_phys, core.cp0_lladdr, llbit as u64]);
        Self(w)
    }
}

/// A proven spin: the lines it reads, stores to or runs from, what they hold
/// at the park point, and the PC range of its code.
pub struct Watch {
    pub lines: Vec<u64>,
    pub contents: Vec<[u64; 8]>,
    pub pcs: (u64, u64),
}

/// Runtime switch: `IRIS_NO_IDLE` turns parking off at startup, the
/// monitor's `idle on|off` for the run.
static IDLE_ENABLED: AtomicBool = AtomicBool::new(true);
static IDLE_INIT: std::sync::Once = std::sync::Once::new();

fn idle_init() {
    IDLE_INIT.call_once(|| {
        if std::env::var_os("IRIS_NO_IDLE").is_some() {
            IDLE_ENABLED.store(false, Ordering::Relaxed);
        }
    });
}

pub fn idle_park_enabled() -> bool {
    idle_init();
    IDLE_ENABLED.load(Ordering::Relaxed)
}

/// `idle on|off`; turning it on wakes a parked thread so the change takes
/// effect at once.
pub fn set_idle_park_enabled(on: bool) {
    idle_init();
    IDLE_ENABLED.store(on, Ordering::Relaxed);
    if on {
        wake();
    }
}

/// Is any processor parked? (Tests wait for it rather than sleeping.)
#[cfg(test)]
pub(crate) fn parked() -> bool {
    PARKED.load(Ordering::SeqCst) != 0
}

/// Tests that park take this first: their cores are all CPU 0.
#[cfg(test)]
pub(crate) static PARK_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::mips_core::{CyclesPtr, CAUSE_IP4, CAUSE_IP7, STATUS_IE, STATUS_IM_SHIFT, STATUS_KSU_SHIFT};

    fn kernel_idle() -> MipsCore {
        let mut core = MipsCore::default();
        core.cp0_status = STATUS_IE | (1 << (STATUS_IM_SHIFT + 7));
        core.cp0_cause = 0;
        core
    }

    /// Offer one full ring of `pcs`.
    fn offered(core: &mut MipsCore, pcs: &[u64]) -> bool {
        let mut st = IdleParkState::ready();
        for &pc in pcs {
            core.pc = pc;
            st.record(core);
        }
        st.should_probe(core, false)
    }

    /// IRIX 6.5's scheduler idle path as the batch sampler sees it: 32
    /// samples over ~600 bytes.
    fn idle_pcs() -> Vec<u64> {
        (0..PC_RING as u64).map(|i| 0xffff_ffff_880d_aedc + i * 19).collect()
    }

    #[test]
    fn a_long_stationary_kernel_loop_is_offered() {
        assert!(offered(&mut kernel_idle(), &idle_pcs()));
    }

    /// Helpers reached by calls form small clusters of their own and do not
    /// hide the loop.
    #[test]
    fn distant_helper_calls_do_not_hide_the_loop() {
        let mut pcs = idle_pcs();
        for (i, pc) in [(3, 0xffff_ffff_8801_ba90), (7, 0xffff_ffff_8801_0300), (11, 0xffff_ffff_8801_ba98), (15, 0xffff_ffff_8801_0318)] {
            pcs[i] = pc;
        }
        assert!(offered(&mut kernel_idle(), &pcs));
    }

    #[test]
    fn short_polls_moving_code_and_user_loops_are_not_offered() {
        let poll: Vec<u64> = (0..PC_RING).map(|i| 0xffff_ffff_8805_604c + 4 * (i % 3) as u64).collect();
        assert!(!offered(&mut kernel_idle(), &poll), "a 3-instruction device poll");
        let moving: Vec<u64> = (0..PC_RING as u64).map(|i| 0xffff_ffff_880d_aedc + i * 0x400).collect();
        assert!(!offered(&mut kernel_idle(), &moving), "code that keeps moving");
        let mut user = kernel_idle();
        user.cp0_status |= 2 << STATUS_KSU_SHIFT;
        assert!(!offered(&mut user, &idle_pcs()), "a user-mode loop");
    }

    #[test]
    fn nothing_is_offered_with_a_line_pending_or_interrupts_off() {
        let mut ready = kernel_idle();
        ready.cp0_cause = CAUSE_IP7;
        assert!(!offered(&mut ready, &idle_pcs()), "an interrupt ready to take");
        let masked = kernel_idle();
        masked.hot.interrupts.store(CAUSE_IP4 as u64, Ordering::SeqCst);
        assert!(!offered(&mut { masked }, &idle_pcs()), "a masked line pending");
        let mut off = kernel_idle();
        off.cp0_status = 1 << (STATUS_IM_SHIFT + 7);
        assert!(!offered(&mut off, &idle_pcs()), "IE clear");
        let mut none = kernel_idle();
        none.cp0_status = STATUS_IE;
        assert!(!offered(&mut none, &idle_pcs()), "every line masked");
    }

    #[test]
    fn a_refusal_backs_off_and_doubles() {
        let mut core = kernel_idle();
        let mut st = IdleParkState::ready();
        st.refused(&core);
        assert!(!st.should_probe(&core, true), "inside the first back-off");
        core.hot.cycles += 64_000;
        assert!(st.should_probe(&core, true));
        st.refused(&core);
        core.hot.cycles += 64_000;
        assert!(!st.should_probe(&core, true), "the second back-off is twice as long");
    }

    #[test]
    fn instructions_that_reach_outside_the_state() {
        let mtc0 = |rd: u32| (0x10 << 26) | (0x04 << 21) | (8 << 16) | (rd << 11);
        let mfc0 = |rd: u32| (0x10 << 26) | (8 << 16) | (rd << 11);
        assert!(!reaches_outside(mtc0(12)), "spl writes Status");
        assert!(reaches_outside(mtc0(11)), "Compare");
        assert!(!reaches_outside(mfc0(12)));
        assert!(reaches_outside(mfc0(9)), "Count changes by itself");
        assert!(reaches_outside(mfc0(1) | (0x01 << 21)), "dmfc0 Random");
        for w in [(0x10 << 26) | (0x10 << 21) | 0x18, (0x10 << 26) | (0x10 << 21) | 0x02, 0x2f << 26, 0x38 << 26, 0x0c] {
            assert!(reaches_outside(w), "{w:#010x}: eret, tlbwi, cache, sc, syscall");
        }
        for w in [0x23 << 26, 0x0f, 0x09 << 26, 0x05 << 26] {
            assert!(!reaches_outside(w), "{w:#010x}: lw, sync, addiu, bne");
        }
    }

    #[test]
    fn only_a_store_that_changes_its_line_counts() {
        let mut p = Probe::default();
        p.note_store(0x2000, [0; 8]);
        p.settle_stores(|_| Some([0; 8]));
        assert_eq!(p.stores, 0, "a store of what was there");
        p.note_store(0x2000, [0; 8]);
        p.settle_stores(|_| Some([1, 0, 0, 0, 0, 0, 0, 0]));
        assert_eq!(p.stores, 1);
        assert_eq!(p.lines, vec![0x2000]);
    }

    #[test]
    fn too_many_lines_refuses() {
        let mut p = Probe::default();
        for i in 0..=MAX_LINES as u64 {
            p.add_line(i * LINE);
        }
        assert_eq!(p.lines.len(), MAX_LINES);
        assert_eq!(p.refusal(), Some(Refusal::TooManyLines));
    }

    #[test]
    fn accesses_are_decoded_with_the_registers_they_run_with() {
        let mut gpr = [0u64; 32];
        gpr[8] = 0xffff_ffff_8000_2000;
        gpr[9] = 0x10;
        let i = |op: u32, imm: u16| (op << 26) | (8 << 21) | (9 << 16) | imm as u32;
        assert_eq!(access(i(0x23, 4), &gpr), Ok(Some((false, 0xffff_ffff_8000_2004))), "lw");
        assert_eq!(access(i(0x2b, 0xfffc), &gpr), Ok(Some((true, 0xffff_ffff_8000_1ffc))), "sw, negative offset");
        assert_eq!(access(i(0x37, 8), &gpr), Ok(Some((false, 0xffff_ffff_8000_2008))), "ld");
        assert_eq!(access(i(0x3d, 0), &gpr), Ok(Some((true, 0xffff_ffff_8000_2000))), "sdc1");
        assert_eq!(access(i(0x22, 1), &gpr), Ok(Some((false, 0xffff_ffff_8000_2001))), "lwl");
        assert_eq!(access((0x13 << 26) | (8 << 21) | (9 << 16) | 0x01, &gpr), Ok(Some((false, 0xffff_ffff_8000_2010))), "ldxc1");
        assert_eq!(access((0x13 << 26) | (8 << 21) | (9 << 16) | 0x08, &gpr), Ok(Some((true, 0xffff_ffff_8000_2010))), "swxc1");
        assert_eq!(access(i(0x33, 0), &gpr), Ok(None), "pref");
        assert_eq!(access(i(0x09, 1), &gpr), Ok(None), "addiu");
        assert!(access(i(0x38, 0), &gpr).is_err(), "sc");
        assert!(access(i(0x2f, 0), &gpr).is_err(), "cache");
    }

    /// Parked time moves the guest-visible cycle counter but retires no
    /// instructions, so the MIPS figure leaves it out; a reset starts both
    /// again.
    #[test]
    fn parked_time_is_not_retired() {
        let mut core = MipsCore::default();
        core.hot.cycles = 1_000;
        let p = CyclesPtr::with_parked(&core.hot.cycles, &core.hot.parked_cycles);
        advance_parked(&mut core, 1_000_000);
        assert_eq!(p.get(), 1_000 + 1_000_000 / STALL_NS_PER_CYCLE);
        assert_eq!(p.retired(), 1_000);
        core.reset(false);
        core.hot.cycles += 50;
        assert_eq!(p.retired(), 50);
        assert_eq!(CyclesPtr::new(&core.hot.cycles).retired(), 50);
    }
}
