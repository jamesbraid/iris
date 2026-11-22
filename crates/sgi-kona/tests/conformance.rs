//! The shared `sgi_engine` clauses (`sgi_engine::conformance`) run on the KONA engine, and the parts of
//! the contract that are KONA's alone: its register widths and what a spent budget leaves midway.

use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options, Region};
use sgi_kona::dev::kona::ident::KonaConfig;
use sgi_engine::conformance::Subject;
use sgi_engine::{BoardEngine, Event, HostMem, Idle, MemFault, Read, Rect, Refused, Source, Swap, Write};

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

fn threaded_engine(notify: Option<Box<dyn Fn() + Send + Sync>>) -> Engine {
    Engine::new(config(), Ram, Options { threaded: true, ..Options::default() }, notify).unwrap()
}

const A: Source = Source { id: 1, charged: true };
const REGION: Region = Region { word: 1, rect: Rect { x0: 0, y0: 0, x1: 3, y1: 3 } };

/// The pipe with its stream run by the caller (`false`) or on its worker (`true`).
struct Kona(bool);

impl Subject for Kona {
    type Engine = Engine;

    fn engine(&self, notify: Option<Box<dyn Fn() + Send + Sync>>) -> Engine {
        if self.0 {
            threaded_engine(notify)
        } else {
            Engine::new(config(), Ram, Options::default(), notify).unwrap()
        }
    }

    fn worker_panics(&self) -> bool {
        self.0
    }

    fn probe(&self) -> u32 {
        INT_VECTORS
    }

    fn store(&self) -> (u32, u64) {
        (COMMAND_PAGE, 1)
    }

    fn port(&self) -> u32 {
        COMMAND_PAGE
    }

    fn region(&self) -> Region {
        REGION
    }

    fn missing_width(&self) -> Option<(u32, u32)> {
        Some((16, INT_VECTORS))
    }

    fn holds_stores(&self) -> bool {
        true
    }

    fn credits(&self) -> bool {
        true
    }

    /// Each textport packet that runs the stream empty raises the low-water cause, which is one
    /// pending-bit packet to the same line.
    fn packet_storm(&self, e: &mut Engine) -> Option<usize> {
        for (off, val) in [(INT_DEST_LOWER, 0x2000), (INT_VECTORS, 0x22), (INT_ENABLE, CAUSE_LOWATER)] {
            e.set_source(A);
            assert_eq!(e.write(off, 32, val as u64), Write::Done);
        }
        for _ in 0..200 {
            for w in [1, 9, 0] {
                e.set_source(A);
                assert_eq!(e.write(COMMAND_PAGE, 32, w), Write::Done);
                e.run(usize::MAX);
            }
        }
        Some(1)
    }

    /// The inline stream runs only under `run` and `wait_idle`; `observe` drains the drawing. A worker's `observe` waits for it.
    fn observe_waits_for_stores(&self) -> bool {
        self.0
    }

    /// Every packet of the storm sets the pending bit; none clears it.
    fn storm_ends_high(&self) -> bool {
        true
    }
}

sgi_engine::conformance_tests!(Kona(false));

/// The same clauses with the stream on its worker.
mod threaded {
    use super::*;

    sgi_engine::conformance_tests!(Kona(true));
}

fn events(e: &mut Engine, max: usize) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, max);
    v
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
    assert!(events(&mut e, usize::MAX).iter().any(|x| matches!(x, Event::Credits { .. })));
}
