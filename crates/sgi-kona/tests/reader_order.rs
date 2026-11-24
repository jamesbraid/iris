//! A credit-level read that helps with the queue, held mid-batch while the worker waits for the pipe, must leave
//! the stream in order: the display-bus writes of 40 commands come out as 0 to 39.

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sgi_engine::{BoardEngine, HostMem, MemFault, Read, Source, Swap, Write};
use sgi_kona::dev::kona::back::display::option::DisplayBusSlave;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

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

#[derive(Default)]
struct Gate {
    held: Mutex<bool>,
    cv: Condvar,
    open: AtomicBool,
}

/// A display-bus device that logs the words written and holds the test's thread in the first one until the gate opens.
struct Board {
    log: Arc<Mutex<Vec<u32>>>,
    gate: Arc<Gate>,
    reader: std::thread::ThreadId,
    held_reader: Arc<AtomicBool>,
}

impl DisplayBusSlave for Board {
    fn read(&mut self, _: u32) -> Option<u32> {
        Some(0)
    }

    fn write(&mut self, addr: u32, data: u32) -> bool {
        if addr != 0xc104 {
            return true;
        }
        let first = {
            let mut log = self.log.lock().unwrap();
            log.push(data);
            log.len() == 1
        };
        if first && std::thread::current().id() == self.reader {
            self.held_reader.store(true, SeqCst);
            let mut held = self.gate.held.lock().unwrap();
            *held = true;
            self.gate.cv.notify_all();
            while !self.gate.open.load(SeqCst) {
                held = self.gate.cv.wait_timeout(held, Duration::from_millis(5)).unwrap().0;
            }
        }
        true
    }
}

// Uncharged, so the 200 stores queue at once behind a worker that has not started.
const A: Source = Source { id: 1, charged: false };

/// One display-bus word write: the video command 1, its address and data, and the checksum.
fn bus_write(e: &mut KonaEngine<Ram>, data: u32) {
    for w in [0x1a6, 1, 0xc104, data, 1 + 0xc104 + data] {
        e.set_source(A);
        while e.write(COMMAND_PAGE, 32, w as u64) == Write::Busy {
            std::thread::yield_now();
        }
    }
}

/// One trial; whether the reader was the thread the gate held.
fn trial() -> bool {
    const COMMANDS: u32 = 40;
    let cfg = KonaConfig::ir2(1).unwrap().with_option_slave().with_display("dg5-2-gvo", 2).unwrap().with_pool(0, 32);
    let mut e = KonaEngine::new(cfg, Ram, Options { threaded: true, ..Options::default() }, None).unwrap();
    let (gate, log) = (Arc::new(Gate::default()), Arc::new(Mutex::new(Vec::new())));
    let held_reader = Arc::new(AtomicBool::new(false));
    let board = Board { log: log.clone(), gate: gate.clone(), reader: std::thread::current().id(), held_reader: held_reader.clone() };
    e.set_display_bus(Some(Box::new(board)));
    for k in 0..COMMANDS {
        bus_write(&mut e, k);
    }
    // Opens the gate 100 ms after the reader is held, by when the worker (woken by the read's flush) waits for the pipe.
    let g = gate.clone();
    let opener = std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut held = g.held.lock().unwrap();
        while !*held && t0.elapsed() < Duration::from_millis(500) {
            held = g.cv.wait_timeout(held, Duration::from_millis(5)).unwrap().0;
        }
        let was_held = *held;
        drop(held);
        if was_held {
            std::thread::sleep(Duration::from_millis(100));
        }
        g.open.store(true, SeqCst);
    });
    assert!(matches!(e.read(CREDIT, 32), Read::Value(_)));
    opener.join().unwrap();
    let t0 = Instant::now();
    while e.queued() != 0 {
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck with {} queued", e.queued());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(*log.lock().unwrap(), (0..COMMANDS).collect::<Vec<_>>(), "display-bus writes out of order");
    e.quiesce(u64::MAX).unwrap();
    held_reader.load(SeqCst)
}

#[test]
fn a_reader_holding_a_batch_while_the_worker_waits_leaves_the_stream_in_order() {
    let forced = (0..6).filter(|_| trial()).count();
    // A trial the worker won proves nothing: require that the reader was held in some, so a pass is not vacuous.
    assert!(forced >= 2, "the reader was held in only {forced} of 6 trials");
}
