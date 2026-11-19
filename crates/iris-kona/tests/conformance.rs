//! The `sgi_engine` contract, as its mock board shows it, checked against the KONA engine: what each
//! entry answers after a fault, the refusal reasons, the event rules, the notify hook and the budgeted
//! quiesce. The mock's own tests are in `crates/sgi-engine/tests/interface.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use iris_kona::dev::kona::defs::*;
use iris_kona::dev::kona::engine::{KonaEngine, Options, Region};
use iris_kona::dev::kona::ident::KonaConfig;
use sgi_engine::{BoardEngine, Chunk, Event, HostMem, Idle, MemFault, Pump, Read, Rect, Refused, Source, Swap, VecSource, Write};

struct Ram;

impl HostMem for Ram {
    fn ram_read(&self, _: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        buf.fill(0);
        Ok(())
    }

    fn ram_write(&self, _: u64, _: &[u8], _: Swap) -> Result<(), MemFault> {
        Ok(())
    }
}

type Engine = KonaEngine<Ram>;

fn config() -> KonaConfig {
    KonaConfig::ir2(1).unwrap().with_pool(0, 32)
}

fn engine() -> Engine {
    Engine::new(config(), Ram, Options::default(), None).unwrap()
}

const A: Source = Source { id: 1, charged: true };
const REGION: Region = Region { word: 1, rect: Rect { x0: 0, y0: 0, x1: 3, y1: 3 } };

fn events(e: &mut Engine, max: usize) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, max);
    v
}

#[test]
fn the_engine_can_be_handed_between_threads() {
    fn send<T: Send>() {}
    send::<Engine>();
}

#[test]
fn a_width_the_registers_lack_is_a_fault_of_the_call_not_of_the_device() {
    let mut e = engine();
    for bits in [8, 16, 24] {
        assert_eq!(e.write(INT_VECTORS, bits, 1), Write::Fault);
        assert_eq!(e.read(INT_VECTORS, bits), Read::Fault);
        assert!(!e.access_waits(COMMAND_PAGE, bits, true));
    }
    assert!(e.fault().is_none());
    assert_eq!(e.write(INT_VECTORS, 32, 0x5a), Write::Done);
    assert_eq!(e.read(INT_VECTORS, 32), Read::Busy, "a register the stream owns waits for it");
    e.run(usize::MAX);
    assert_eq!(e.read(INT_VECTORS, 32), Read::Value(0x5a));
    // A 64-bit store to the command page is two words.
    assert_eq!(e.write(COMMAND_PAGE, 64, 0x1_0000_0001), Write::Done);
    assert_eq!(e.queued(), 2);
}

#[test]
fn a_faulted_engine_answers_as_the_contract_says_on_every_entry() {
    let mut e = engine();
    e.set_source(A);
    assert_eq!(e.write(COMMAND_PAGE, 32, 1), Write::Done);
    e.fail("the shell's glue panicked");
    e.fail("a second fault");
    assert_eq!(e.fault().as_deref(), Some("the shell's glue panicked"), "the first fault stays");
    assert_eq!(events(&mut e, usize::MAX).iter().filter(|x| matches!(x, Event::DeviceFault(_))).count(), 1);

    assert_eq!(e.write(INT_VECTORS, 32, 1), Write::Fault);
    assert_eq!(e.read(INT_VECTORS, 32), Read::Fault);
    assert_eq!(e.push_words(COMMAND_PAGE, &[1, 2]), 0);
    assert_eq!(e.run(usize::MAX), 0);
    assert!(!e.access_waits(INT_VECTORS, 32, false) && !e.access_waits(COMMAND_PAGE, 32, true));
    assert_eq!(e.wait_idle(0, 0), Idle::Faulted);
    assert_eq!(e.advance(0).idle, Idle::Faulted);
    assert!(e.observe(&REGION).is_err());
    assert!(matches!(e.observe_all(), Err(Refused::Faulted(_))), "before it checks quiescence");
    assert!(matches!(e.save(&mut Vec::<Chunk>::new()), Err(Refused::Faulted(_))));
    assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::Faulted(_))));
    assert!(matches!(e.quiesce(0), Err(Refused::Faulted(_))), "and still parks");
    assert_eq!(e.irq_levels(), 0);
    e.resume();
}

#[test]
fn a_reset_clears_a_fault_and_posts_one_irq() {
    let mut e = engine();
    e.fail("down");
    events(&mut e, usize::MAX);
    e.reset().unwrap();
    assert!(e.fault().is_none());
    let seen = events(&mut e, usize::MAX);
    assert_eq!(seen.iter().filter(|x| **x == Event::Irq).count(), 1, "{seen:?}");
    assert_eq!(e.write(INT_VECTORS, 32, 7), Write::Done);
}

#[test]
fn each_refusal_has_its_own_reason() {
    let mut e = engine();
    // Not parked: nothing may save, load, observe everything or be trusted at rest.
    assert!(matches!(e.observe_all(), Err(Refused::NotQuiesced)));
    assert!(matches!(e.save(&mut Vec::<Chunk>::new()), Err(Refused::NotQuiesced)));
    assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::NotQuiesced)));
    // Stores wait: not a reset either.
    e.set_source(A);
    e.write(COMMAND_PAGE, 32, 1);
    assert!(matches!(e.reset(), Err(Refused::NotQuiesced)));
    e.quiesce(u64::MAX).unwrap();
    e.observe_all().unwrap();
    assert!(matches!(e.load(&mut VecSource::new(vec![])), Err(Refused::Mismatch(_))));
    e.reset().unwrap();
}

#[test]
fn a_budget_that_runs_out_is_midway_and_the_engine_goes_on() {
    let mut e = engine();
    e.set_source(A);
    // Words the textport takes as sync words: 10,000 stores to run.
    for _ in 0..10_000 {
        assert_eq!(e.write(COMMAND_PAGE, 32, 1), Write::Done);
    }
    assert!(matches!(e.quiesce(0), Err(Refused::Midway(_))));
    assert!(e.advance(0).idle != Idle::Yes, "it is still running, not parked");
    e.quiesce(u64::MAX).unwrap();
    assert_eq!(e.wait_idle(0, 0), Idle::Yes);
}

#[test]
fn credits_coalesce_per_source_and_events_drain_in_pieces() {
    let mut e = engine();
    for id in [1u32, 2, 3] {
        e.set_source(Source { id, charged: true });
        for _ in 0..100 {
            e.write(COMMAND_PAGE, 32, 1);
        }
        e.run(usize::MAX);
    }
    assert!(events(&mut e, 0).is_empty());
    let first = events(&mut e, 1);
    assert_eq!(first.len(), 1);
    let rest = events(&mut e, usize::MAX);
    let total: u32 = first.iter().chain(&rest).filter_map(|x| if let Event::Credits { retired, .. } = x { Some(*retired) } else { None }).sum();
    assert_eq!((total, first.len() + rest.len()), (300, 3), "one event for each source, 100 credits each");
}

#[test]
fn notify_fires_once_until_the_pump_has_drained() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let mut e = Engine::new(config(), Ram, Options::default(), Some(Box::new(move || drop(c.fetch_add(1, Ordering::SeqCst))))).unwrap();
    e.set_source(A);
    e.write(COMMAND_PAGE, 32, 1);
    e.write(COMMAND_PAGE, 32, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "queued work wakes the shell once");
    let Pump { events, next_deadline, irq_levels, .. } = e.advance(0);
    assert_eq!((events, next_deadline, irq_levels), (false, None, 0));
    e.run(usize::MAX);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the next batch wakes it again");
}
