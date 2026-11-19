//! The VPro engine through `BoardEngine`, as a shell drives it: identity,
//! lent memory and its dirty runs, interrupt packets, credits per source, the
//! fence's write-back before its interrupt, quiesce, resume and refusals, the
//! state stream, and a faulted engine on every entry. Registers are the
//! aperture offsets a driver uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use iris_vpro::dev::odyssey::engine::{memory_bytes, Config, Mode, VproEngine};
use parking_lot::{Condvar, Mutex};
use sgi_engine::*;

const CMD_FIFO: u32 = 0x110000;
const INTR_CLEAR: u32 = 0x101c;
const INTR_ENABLE_SET: u32 = 0x1034;
const ROUTE_DEST_UPPER: u32 = 0x1044;
const ROUTE_DEST_LOWER: u32 = 0x104c;
const ROUTE_VECTORS: u32 = 0x1054;
const HOST_WRITE_UPPER: u32 = 0x4034;
const HOST_WRITE_LOWER: u32 = 0x403c;
const LOW_WATER: u64 = 0x200;
const FLAG_VECTOR: u8 = 18;
const RETRACE_VECTOR: u8 = 20;
const SOFTSYNC_VECTOR: u8 = 21;
const HEART: u8 = 8;

/// Guest RAM, big-endian bytes in sparse pages, with one address a writer can be held at
/// and a size past which nothing is mapped.
struct Ram {
    pages: Mutex<HashMap<u64, Box<[u8; 4096]>>>,
    hold: Mutex<Option<u64>>,
    open: Condvar,
    held: AtomicBool,
    top: AtomicU64,
}

impl Default for Ram {
    fn default() -> Ram {
        Ram { pages: Default::default(), hold: Default::default(), open: Default::default(), held: Default::default(), top: AtomicU64::new(u64::MAX) }
    }
}

impl Ram {
    fn half(&self, addr: u64) -> u16 {
        let p = self.pages.lock();
        let at = |a: u64| p.get(&(a / 4096)).map_or(0, |pg| pg[(a % 4096) as usize]);
        u16::from_be_bytes([at(addr), at(addr + 1)])
    }

    /// Block any write that covers `addr` until `release`.
    fn hold_writes_at(&self, addr: u64) {
        *self.hold.lock() = Some(addr);
    }

    fn release(&self) {
        *self.hold.lock() = None;
        self.open.notify_all();
    }
}

struct Shared(Arc<Ram>);

impl HostMem for Shared {
    fn ram_read(&self, addr: u64, out: &mut [u8], _swap: Swap) -> Result<(), MemFault> {
        let p = self.0.pages.lock();
        for (k, b) in out.iter_mut().enumerate() {
            let a = addr + k as u64;
            *b = p.get(&(a / 4096)).map_or(0, |pg| pg[(a % 4096) as usize]);
        }
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _swap: Swap) -> Result<(), MemFault> {
        if addr + data.len() as u64 > self.0.top.load(Ordering::SeqCst) {
            return Err(MemFault);
        }
        let mut hold = self.0.hold.lock();
        while hold.is_some_and(|h| (addr..addr + data.len() as u64).contains(&h)) {
            self.0.held.store(true, Ordering::SeqCst);
            self.0.open.wait(&mut hold);
        }
        self.0.held.store(false, Ordering::SeqCst);
        drop(hold);
        let mut p = self.0.pages.lock();
        for (k, b) in data.iter().enumerate() {
            let a = addr + k as u64;
            p.entry(a / 4096).or_insert_with(|| Box::new([0; 4096]))[(a % 4096) as usize] = *b;
        }
        Ok(())
    }
}

fn config(product: &str, mode: Mode) -> Config {
    Config { product: product.into(), widget: 11, mode, pool: (0, 0), gpu: None }
}

fn engine(mode: Mode) -> (VproEngine, Arc<Ram>) {
    let ram = Arc::new(Ram::default());
    (VproEngine::new(config("v12", mode), None, Shared(ram.clone()), None).unwrap(), ram)
}

fn put(e: &mut impl BoardEngine, off: u32, val: u64) {
    loop {
        match e.write(off, 32, val) {
            Write::Done => return,
            Write::Busy => std::thread::yield_now(),
            Write::Fault => panic!("write {off:#x}: {:?}", e.fault()),
        }
    }
}

fn get(e: &mut impl BoardEngine, off: u32) -> u64 {
    loop {
        match e.read(off, 32) {
            Read::Value(v) => return v,
            Read::Busy => std::thread::yield_now(),
            Read::Fault => panic!("read {off:#x}: {:?}", e.fault()),
        }
    }
}

fn drain(e: &mut impl BoardEngine<Region = Rect>) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, usize::MAX);
    v
}

/// Route the four interrupt sources to HEART's packet register, as the kernel does.
fn route_to_heart(e: &mut impl BoardEngine) {
    put(e, ROUTE_DEST_UPPER, (HEART as u64) << 25 | 7 << 16);
    put(e, ROUTE_DEST_LOWER, 0x80);
    put(e, ROUTE_VECTORS, 21 << 24 | 20 << 16 | 19 << 8 | 18);
}

fn packet(vector: u8, level: bool) -> Event {
    Event::Interrupt(Interrupt { widget: HEART, addr: 0x80, vector, level })
}

/// The first reads of every recorded V12 boot (QEMU's capture of an Octane2).
#[test]
fn v12_identity_and_widget() {
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode);
        assert_eq!(get(&mut e, 0x4), 0xac01_3047);
        assert_eq!(get(&mut e, 0x24), 0xb);
        assert_eq!(e.read(0x4, 24), Read::Fault, "a width the aperture does not have");
    }
}

#[test]
fn lent_memory_is_zeroed_and_reported_whole_once_then_size_checked() {
    let bytes = memory_bytes("v12").unwrap();
    assert_eq!(bytes, 128 << 20);
    let mut mem = vec![!0u64; (bytes / 8) as usize];
    let lent = unsafe { Lent::new(mem.as_mut_ptr().cast(), bytes as usize) }.unwrap();
    let mut e = VproEngine::new(config("v12", Mode::Inline), Some(lent), Shared(Default::default()), None).unwrap();
    let (mut runs, mut again) = (Vec::new(), Vec::new());
    e.drain_dirty(&mut runs);
    e.drain_dirty(&mut again);
    assert_eq!(runs, vec![DirtyRun { offset: 0, len: bytes }], "the power-on clear wrote all of it");
    assert!(again.is_empty(), "drained once");
    e.trapped(&mut again);
    assert!(again.is_empty(), "no GPU unit, nothing trapped");
    drop(e);
    assert!(mem.iter().all(|&w| w == 0));

    let short = unsafe { Lent::new(mem.as_mut_ptr().cast(), 32 << 20) }.unwrap();
    assert!(VproEngine::new(config("v12", Mode::Inline), Some(short), Shared(Default::default()), None).is_err());
}

#[test]
fn a_flag_that_rises_and_falls_before_the_drain_is_one_packet_and_retrace_comes_from_the_pump() {
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode);
        route_to_heart(&mut e);
        // The empty FIFO leaves low water pending, so enabling it raises the line.
        put(&mut e, INTR_ENABLE_SET, LOW_WATER);
        put(&mut e, INTR_CLEAR, LOW_WATER);
        assert_eq!(drain(&mut e), vec![packet(FLAG_VECTOR, false)], "the newest state of the line, not its history");
        assert_eq!((e.irq_levels(), e.advance(1_000_000_000).irq_levels), (0, 0), "packets, not levels");
        let p = e.advance(1_000_000_000 + 20_000_000);
        assert!(p.events && p.next_deadline.is_some(), "{p:?}");
        assert!(drain(&mut e).contains(&packet(RETRACE_VECTOR, true)));
    }
}

#[test]
fn credits_go_to_the_source_of_the_store_and_coalesce() {
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode);
        e.set_source(Source { id: 7, charged: true });
        assert_eq!(e.push_words(CMD_FIFO, &[0; 1000]), 1000);
        e.set_source(Source { id: 9, charged: true });
        put(&mut e, CMD_FIFO, 0);
        e.set_source(Source { id: 11, charged: false });
        put(&mut e, CMD_FIFO, 0);
        assert_eq!(e.push_words(0x1000, &[0]), 0, "not a FIFO port");
        assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
        assert!(e.advance(1).events);
        assert_eq!(drain(&mut e), vec![Event::Credits { source: 7, retired: 1000 }, Event::Credits { source: 9, retired: 1 }], "a store that took no credit returns none");
        assert!(!e.advance(2).events, "returned once");
    }
}

/// The fence's write-back lands before the interrupt that announces it: the
/// raster thread is held inside the write, and no packet is out yet.
#[test]
fn a_fence_raises_its_interrupt_only_after_its_write_back() {
    let (mut e, ram) = engine(Mode::Threaded);
    route_to_heart(&mut e);
    put(&mut e, HOST_WRITE_UPPER, 0);
    put(&mut e, HOST_WRITE_LOWER, 0x10000);
    drain(&mut e);
    ram.hold_writes_at(0x10000 + 2 * 5);
    // The host-write command: its register select, the slot with the interrupt bit, the token.
    e.push_words(CMD_FIFO, &[0x0001_0a5a, 5 | 1 << 15, 0xbeef]);
    let t = std::time::Instant::now();
    while !ram.held.load(Ordering::SeqCst) {
        assert!(t.elapsed().as_secs() < 10, "the fence never reached its write");
        std::thread::yield_now();
    }
    assert!(!drain(&mut e).contains(&packet(SOFTSYNC_VECTOR, true)), "the interrupt waited for the write");
    ram.release();
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    assert_eq!(ram.half(0x10000 + 2 * 5), 0xbeef);
    assert!(drain(&mut e).contains(&packet(SOFTSYNC_VECTOR, true)));
}

/// A fence whose slot is not RAM is reported once, as a write the DMA could not make.
#[test]
fn dma_that_reaches_no_ram_is_reported_once_a_page() {
    let (mut e, ram) = engine(Mode::Threaded);
    ram.top.store(0x8000, Ordering::SeqCst);
    put(&mut e, HOST_WRITE_UPPER, 0);
    put(&mut e, HOST_WRITE_LOWER, 0x10000);
    for (slot, token) in [(1, 0x1111), (2, 0x2222)] {
        e.push_words(CMD_FIFO, &[0x0001_0a5a, slot, token]);
    }
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    let faults: Vec<Event> = drain(&mut e).into_iter().filter(|x| matches!(x, Event::BusFault { .. })).collect();
    assert_eq!(faults.len(), 1, "two writes to one page: {faults:?}");
    assert!(matches!(faults[0], Event::BusFault { addr, write: true } if (0x10000..0x11000).contains(&addr)), "{faults:?}");
    assert_eq!(ram.half(0x10002), 0, "nothing was written");
}

#[test]
fn quiesce_parks_the_threads_and_resume_restarts_them_and_each_refusal_has_its_reason() {
    let (mut e, _) = engine(Mode::Threaded);
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::NotQuiesced), "the threads run");
    assert_eq!(e.reset(), Err(Refused::NotQuiesced));
    assert_eq!(e.observe_all(), Err(Refused::NotQuiesced));
    e.push_words(CMD_FIFO, &[0; 5000]);
    e.quiesce(u64::MAX).unwrap();
    e.observe_all().unwrap();
    let mut chunks = Vec::new();
    e.save(&mut chunks).unwrap();
    assert_eq!(chunks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["meta", "state"], "zero memory is left out");
    assert_eq!(e.advance(1).idle, Idle::Yes);
    // A store while parked runs on the caller's thread and is visible at once.
    put(&mut e, INTR_ENABLE_SET, 0x1234);
    assert_eq!(get(&mut e, INTR_ENABLE_SET), 0x1234);
    e.resume();
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::NotQuiesced), "running again");
    put(&mut e, CMD_FIFO, 0);
    e.quiesce(u64::MAX).unwrap();
    e.reset().unwrap();
    assert_eq!(get(&mut e, INTR_ENABLE_SET), 0, "power-on state");
    assert_eq!(e.state_version(), 1);

    let mismatch = |e: &mut dyn BoardEngine<Region = Rect>, c: Vec<Chunk>| matches!(e.load(&mut VecSource::new(c)), Err(Refused::Mismatch(_)));
    assert!(mismatch(&mut e, vec![]), "an empty state");
    assert!(mismatch(&mut e, vec![chunks[1].clone()]), "meta is not first");
    assert!(mismatch(&mut e, vec![chunks[0].clone()]), "no board state");
    let mut twice = chunks.clone();
    twice.push(chunks[1].clone());
    assert!(mismatch(&mut e, twice), "two board states");
    let mut other = chunks.clone();
    other.push(Chunk { name: "extra".into(), index: 0, bytes: vec![] });
    assert!(mismatch(&mut e, other), "a part nobody knows");
    let mut cut = chunks.clone();
    cut[1].bytes.truncate(40);
    assert!(mismatch(&mut e, cut), "a state cut short");
    e.reset().unwrap();
}

#[test]
fn state_round_trips_and_a_board_of_another_product_or_memory_refuses_it() {
    let bytes = memory_bytes("v12").unwrap() as usize;
    let mut buf = vec![0u64; bytes / 8];
    let lent = |b: &mut Vec<u64>| unsafe { Lent::new(b.as_mut_ptr().cast(), bytes) }.unwrap();
    let mut a = VproEngine::new(config("v12", Mode::Inline), Some(lent(&mut buf)), Shared(Default::default()), None).unwrap();
    route_to_heart(&mut a);
    put(&mut a, INTR_ENABLE_SET, 0xff00);
    let mut chunks = Vec::new();
    a.save(&mut chunks).unwrap();
    assert_eq!(chunks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["meta", "state"], "lent memory is the shell's to save");

    let mut buf2 = vec![0u64; bytes / 8];
    let mut b = VproEngine::new(config("v12", Mode::Inline), Some(lent(&mut buf2)), Shared(Default::default()), None).unwrap();
    b.load(&mut VecSource::new(chunks.clone())).unwrap();
    assert_eq!(get(&mut b, INTR_ENABLE_SET), 0xff00);
    assert!(drain(&mut b).contains(&packet(FLAG_VECTOR, true)), "the routing and the raised flag came across, and the level is sent again");

    let mut own = VproEngine::new(config("v12", Mode::Inline), None, Shared(Default::default()), None).unwrap();
    let Err(Refused::Mismatch(why)) = own.load(&mut VecSource::new(chunks.clone())) else { panic!("an engine with its own memory took a lent state") };
    assert!(why.contains("board memory"), "{why}");
    let mut v10 = VproEngine::new(config("v10", Mode::Inline), None, Shared(Default::default()), None).unwrap();
    let mut own_chunks = Vec::new();
    own.save(&mut own_chunks).unwrap();
    let Err(Refused::Mismatch(why)) = v10.load(&mut VecSource::new(own_chunks)) else { panic!("a v10 took a v12's state") };
    assert!(why.contains("v10"), "{why}");
}

#[test]
fn a_faulted_engine_answers_as_the_contract_says_on_every_entry() {
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode);
        route_to_heart(&mut e);
        e.fail("the board stopped");
        e.fail("a second fault");
        assert_eq!(e.fault().as_deref(), Some("the board stopped"), "the first fault stays");
        assert_eq!(e.write(CMD_FIFO, 32, 0), Write::Fault);
        assert_eq!(e.push_words(CMD_FIFO, &[0, 0]), 0);
        assert_eq!(e.read(0x4, 32), Read::Fault);
        assert!(!e.access_waits(0x4, 32, false) && !e.access_waits(CMD_FIFO, 32, true));
        assert_eq!(e.run(10), 0);
        assert_eq!(e.advance(1).idle, Idle::Faulted);
        assert_eq!(e.wait_idle(1, 1), Idle::Faulted);
        let why = Fault("the board stopped".into());
        assert_eq!(e.observe(&Rect::new(0, 0, 8, 8)), Err(why.clone()));
        assert_eq!(e.observe_lent(0, 8), Err(why));
        let faulted = Refused::Faulted("the board stopped".into());
        assert_eq!(e.observe_all(), Err(faulted.clone()));
        assert_eq!(e.save(&mut Vec::new()), Err(faulted.clone()));
        assert_eq!(e.load(&mut VecSource::new(vec![])), Err(faulted.clone()));
        assert_eq!(e.quiesce(1), Err(faulted), "it reports the fault and still parks");
        assert_eq!(e.irq_levels(), 0);
        assert!(drain(&mut e).contains(&Event::DeviceFault("the board stopped".into())));
        assert!(!drain(&mut e).contains(&Event::DeviceFault("the board stopped".into())), "once");
        e.reset().unwrap();
        e.resume();
        assert_eq!((e.fault(), e.write(CMD_FIFO, 32, 0)), (None, Write::Done));
        assert!(drain(&mut e).is_empty(), "a reset drops what waited");
    }
}

/// A handle moves between threads, one call at a time.
#[test]
fn an_engine_moves_between_threads() {
    let (mut e, _) = engine(Mode::Threaded);
    put(&mut e, INTR_ENABLE_SET, 0x42);
    let mut e = std::thread::spawn(move || {
        put(&mut e, INTR_ENABLE_SET, 0x43);
        e
    })
    .join()
    .unwrap();
    assert_eq!(get(&mut e, INTR_ENABLE_SET), 0x42 | 0x43);
}

/// With a GPU unit the memory the shell lent is the unit's mirror: all of it is
/// trapped, and a CPU write is taken in place of the unit's copy.
#[test]
fn a_gpu_unit_traps_the_lent_memory() {
    let bytes = memory_bytes("v12").unwrap() as usize;
    let mut buf = vec![0u64; bytes / 8];
    let lent = unsafe { Lent::new(buf.as_mut_ptr().cast(), bytes) }.unwrap();
    let mut cfg = config("v12", Mode::Inline);
    cfg.gpu = Some("auto".into());
    let mut e = match VproEngine::new(cfg, Some(lent), Shared(Default::default()), None) {
        Ok(e) => e,
        Err(why) => {
            eprintln!("skipped: {why}");
            return;
        }
    };
    let mut t = Vec::new();
    e.trapped(&mut t);
    assert_eq!(t, vec![DirtyRun { offset: 0, len: bytes as u64 }]);
    e.cpu_wrote(0, 8);
    e.observe_lent(0, 8).unwrap();
    e.quiesce(u64::MAX).unwrap();
    e.observe_all().unwrap();
    assert_eq!(e.fault(), None);
}
