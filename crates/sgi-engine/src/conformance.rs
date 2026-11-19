//! The contract's board-neutral clauses, written once for every board.
//!
//! A board's test file implements `Subject` (how to build an engine and which
//! register, store and port to use) and calls `conformance_tests!`, which
//! makes one `#[test]` for each clause below. The clauses use no register
//! the contract does not name, so a board that fails one has a bug in the
//! board or in the contract, not in the test. What only one board has (a
//! register's meaning, a stall, lent memory) stays in that board's own tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::{BoardEngine, Chunk, Event, Idle, Read, Refused, Source, VecSource, Write};

/// What a board tells the clauses about itself.
pub trait Subject {
    type Engine: BoardEngine + Send + 'static;

    /// A fresh engine in its default mode, with the shell's notify hook.
    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Self::Engine;
    /// A register `read` answers at 32 bits.
    fn probe(&self) -> u32;
    /// A 32-bit store the board takes at this offset.
    fn store(&self) -> (u32, u64);
    /// The port `push_words` takes.
    fn port(&self) -> u32;
    /// A box for `observe`.
    fn region(&self) -> <Self::Engine as BoardEngine>::Region;
    /// A width (and an offset it was tried at) the board's registers lack and
    /// answer with `Fault`; None for a board that widens every access.
    fn missing_width(&self) -> Option<(u32, u32)>;
    /// Whether a store is still queued, or a worker still runs, after `write`
    /// returns: until `quiesce`, `save`, `load` and `reset` are refused.
    fn holds_stores(&self) -> bool;
    /// Whether a store from a charged source comes back as `Event::Credits`.
    fn credits(&self) -> bool;
    /// Raise one interrupt line over and over without draining, and say how
    /// many distinct lines that was; None for a board with no packets.
    fn packet_storm(&self, _e: &mut Self::Engine) -> Option<usize> {
        None
    }
}

const CHARGED: fn(u32) -> Source = |id| Source { id, charged: true };

fn put<S: Subject>(s: &S, e: &mut S::Engine, n: usize) {
    let (off, val) = s.store();
    for _ in 0..n {
        loop {
            match e.write(off, 32, val) {
                Write::Done => break,
                Write::Busy => {
                    e.run(usize::MAX);
                    e.advance(0);
                    std::thread::yield_now();
                }
                Write::Fault => panic!("write {off:#x}: {:?}", e.fault()),
            }
        }
    }
}

/// Run everything queued, park the engine and let the pump return what is owed.
fn settle<E: BoardEngine>(e: &mut E) {
    let mut now = 0;
    crate::shell::quiesce(e, &mut now, 1_000_000, 100_000).expect("the engine settles");
    e.advance(now);
}

fn events<E: BoardEngine>(e: &mut E, max: usize) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, max);
    v
}

fn credits_of(ev: &[Event]) -> Vec<(u32, u32)> {
    ev.iter().filter_map(|x| if let Event::Credits { source, retired } = x { Some((*source, *retired)) } else { None }).collect()
}

pub fn send<S: Subject>(_: &S) {
    fn is_send<T: Send>() {}
    is_send::<S::Engine>();
}

pub fn missing_width<S: Subject>(s: &S) {
    let Some((bits, off)) = s.missing_width() else { return };
    let mut e = s.engine(None);
    assert_eq!(e.write(off, bits, 1), Write::Fault);
    assert_eq!(e.read(off, bits), Read::Fault);
    assert!(!e.access_waits(off, bits, true));
    assert!(e.fault().is_none(), "a width the registers lack is a fault of the call, not of the device");
    let (store, val) = s.store();
    assert_eq!(e.write(store, 32, val), Write::Done);
}

pub fn faulted_entries<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    let (off, val) = s.store();
    e.set_source(CHARGED(1));
    assert_eq!(e.write(off, 32, val), Write::Done);
    e.fail("the shell's glue panicked");
    e.fail("a second fault");
    assert_eq!(e.fault().as_deref(), Some("the shell's glue panicked"), "the first fault stays");
    let seen = events(&mut e, usize::MAX);
    assert_eq!(seen.iter().filter(|x| matches!(x, Event::DeviceFault(_))).count(), 1, "{seen:?}");

    assert_eq!(e.write(off, 32, val), Write::Fault);
    assert_eq!(e.read(s.probe(), 32), Read::Fault);
    assert_eq!(e.push_words(s.port(), &[1, 2]), 0);
    assert_eq!(e.run(usize::MAX), 0);
    assert!(!e.access_waits(s.probe(), 32, false) && !e.access_waits(off, 32, true));
    assert_eq!(e.wait_idle(0, 0), Idle::Faulted);
    assert_eq!(e.advance(0).idle, Idle::Faulted);
    assert!(e.observe(&s.region()).is_err());
    assert!(matches!(e.observe_all(), Err(Refused::Faulted(_))), "before it checks quiescence");
    assert!(matches!(e.save(&mut Vec::<Chunk>::new()), Err(Refused::Faulted(_))));
    assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::Faulted(_))));
    assert!(matches!(e.quiesce(0), Err(Refused::Faulted(_))), "and still parks");
    let _ = e.irq_levels();
    e.resume();
}

pub fn reset_clears_a_fault_and_posts_one_irq<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    e.fail("down");
    assert!(matches!(e.quiesce(u64::MAX), Err(Refused::Faulted(_))), "a faulted engine parks, and says so");
    events(&mut e, usize::MAX);
    e.reset().unwrap();
    assert!(e.fault().is_none());
    let seen = events(&mut e, usize::MAX);
    assert_eq!(seen.iter().filter(|x| **x == Event::Irq).count(), 1, "{seen:?}");
    let (off, val) = s.store();
    assert_eq!(e.write(off, 32, val), Write::Done);
}

pub fn refusals<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    let (off, val) = s.store();
    if s.holds_stores() {
        assert_eq!(e.write(off, 32, val), Write::Done);
        assert!(matches!(e.save(&mut Vec::<Chunk>::new()), Err(Refused::NotQuiesced)));
        assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::NotQuiesced)));
        assert!(matches!(e.observe_all(), Err(Refused::NotQuiesced)));
        assert!(matches!(e.reset(), Err(Refused::NotQuiesced)));
    }
    settle(&mut e);
    e.observe_all().unwrap();
    e.save(&mut Vec::<Chunk>::new()).unwrap();
    assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::Mismatch(_))), "an empty state fits no engine");
    e.reset().unwrap();
}

pub fn state_round_trips<S: Subject>(s: &S) {
    let mut a = s.engine(None);
    put(s, &mut a, 3);
    settle(&mut a);
    let mut chunks = Vec::new();
    a.save(&mut chunks).unwrap();
    assert_eq!(chunks.first().map(|c| c.name.as_str()), Some("meta"), "meta comes first");

    let mut b = s.engine(None);
    settle(&mut b);
    b.load(&mut VecSource::new(chunks.clone())).unwrap();
    let mut again = Vec::new();
    b.save(&mut again).unwrap();
    assert_eq!(again, chunks, "a loaded state saves as it was");
    assert_eq!(a.state_version(), b.state_version());
}

pub fn a_bad_state_is_refused_and_reset_recovers<S: Subject>(s: &S) {
    let mut a = s.engine(None);
    settle(&mut a);
    let mut chunks = Vec::new();
    a.save(&mut chunks).unwrap();

    let mut b = s.engine(None);
    settle(&mut b);
    let mut torn = chunks.clone();
    torn[0].bytes[0] = torn[0].bytes[0].wrapping_add(1);
    assert!(b.load(&mut VecSource::new(torn)).is_err(), "a state of another version");
    b.reset().unwrap();
    let (off, val) = s.store();
    assert_eq!(b.write(off, 32, val), Write::Done, "after a refused load the shell resets and goes on");
}

pub fn credits_coalesce_per_source_and_events_drain_in_pieces<S: Subject>(s: &S) {
    if !s.credits() {
        return;
    }
    let mut e = s.engine(None);
    for id in [1u32, 2, 3] {
        e.set_source(CHARGED(id));
        put(s, &mut e, 100);
    }
    e.set_source(Source { id: 9, charged: false });
    put(s, &mut e, 10);
    settle(&mut e);

    assert!(events(&mut e, 0).is_empty());
    let first = events(&mut e, 1);
    assert_eq!(first.len(), 1);
    let rest = events(&mut e, usize::MAX);
    let mut all: Vec<Event> = first;
    all.extend(rest);
    let got = credits_of(&all);
    // A board may settle a credit on the store that completes a command, so the split between
    // two sources can move by a store; the total and the coalescing may not.
    let mut sources: Vec<u32> = got.iter().map(|c| c.0).collect();
    sources.sort();
    sources.dedup();
    assert_eq!(sources.len(), got.len(), "one event for each source: {all:?}");
    assert!(sources.iter().all(|id| [1, 2, 3].contains(id)), "none for the uncharged source: {all:?}");
    assert_eq!(got.iter().map(|c| c.1).sum::<u32>(), 300, "a credit for each charged store: {all:?}");
}

pub fn notify_fires_once_until_the_pump_has_drained<S: Subject>(s: &S) {
    if !s.credits() {
        return;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mut e = s.engine(Some(Box::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
    })));
    e.set_source(CHARGED(1));
    put(s, &mut e, 2);
    settle(&mut e);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a batch of work and events wakes the shell once");
    events(&mut e, usize::MAX);
    e.resume();
    put(s, &mut e, 1);
    settle(&mut e);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the next batch wakes it again");
}

pub fn a_budget_that_runs_out_leaves_the_engine_going<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    put(s, &mut e, 10_000);
    // Done at once, or midway; never an error, and the engine finishes.
    match e.quiesce(0) {
        Ok(()) => e.resume(),
        Err(Refused::Midway(_)) => assert_ne!(e.advance(0).idle, Idle::Faulted),
        Err(other) => panic!("a budget of nothing: {other}"),
    }
    settle(&mut e);
    assert_eq!(e.wait_idle(0, 0), Idle::Yes);
}

pub fn a_storm_on_one_interrupt_line_stays_one_packet<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    let Some(lines) = s.packet_storm(&mut e) else { return };
    settle(&mut e);
    let seen = events(&mut e, usize::MAX);
    let packets = seen.iter().filter(|x| matches!(x, Event::Interrupt(_))).count();
    assert!(packets >= 1 && packets <= lines, "{packets} packets for {lines} lines: {seen:?}");
}

pub fn observe_brings_a_box_current<S: Subject>(s: &S) {
    let mut e = s.engine(None);
    put(s, &mut e, 1);
    settle(&mut e);
    e.observe(&s.region()).unwrap();
}

/// One `#[test]` for each clause, for a board's test file.
#[macro_export]
macro_rules! conformance_tests {
    ($subject:expr) => {
        $crate::conformance_tests!(@each $subject;
            send,
            missing_width,
            faulted_entries,
            reset_clears_a_fault_and_posts_one_irq,
            refusals,
            state_round_trips,
            a_bad_state_is_refused_and_reset_recovers,
            credits_coalesce_per_source_and_events_drain_in_pieces,
            notify_fires_once_until_the_pump_has_drained,
            a_budget_that_runs_out_leaves_the_engine_going,
            a_storm_on_one_interrupt_line_stays_one_packet,
            observe_brings_a_box_current
        );
    };
    (@each $subject:expr; $($clause:ident),*) => {
        $( #[test] fn $clause() { $crate::conformance::$clause(&$subject); } )*
    };
}
