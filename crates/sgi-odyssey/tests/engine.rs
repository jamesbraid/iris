//! The VPro engine through `BoardEngine`, as a shell drives it: identity,
//! lent memory and its dirty runs, interrupt packets, credits per source, the
//! fence's write-back before its interrupt, quiesce, resume and refusals, the
//! state stream, and a faulted engine on every entry. Registers are the
//! aperture offsets a driver uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use sgi_odyssey::dev::odyssey::engine::{memory_bytes, Config, Mode, VproEngine};
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
const WIDGET: u32 = 11;
/// The display back end's word FIFO, and the display-mode table in its register file.
const BACKEND_SELECT: u32 = 0x400000;
const DISPMODE_BASE: u32 = 0x2900;

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
    Config { product: product.into(), widget: WIDGET, mode, pool: (0, 0), params: Default::default(), gpu: None }
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
    Event::Interrupt(Interrupt { sender: WIDGET as u8, widget: HEART, addr: 0x80, vector, level })
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

/// A store that only joins the queue is taken without a call to `write`; one that earns a credit, one
/// for another register, and an inline board's store (which would run in the call) are left to `write`.
#[test]
fn try_store_takes_only_a_plain_push_on_a_board_with_a_thread() {
    let (mut e, _) = engine(Mode::Threaded);
    e.set_source(Source { id: 1, charged: false });
    assert_eq!(e.try_store(CMD_FIFO, 32, 0), Write::Done);
    assert_eq!(e.try_store(CMD_FIFO, 64, 0), Write::Done);
    assert_eq!(e.try_store(INTR_CLEAR, 32, 0), Write::Busy, "not a FIFO port");
    e.set_source(Source { id: 2, charged: true });
    assert_eq!(e.try_store(CMD_FIFO, 32, 0), Write::Busy, "it earns a credit");
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    e.advance(1);
    assert_eq!(drain(&mut e), vec![], "an uncharged store returns no credit");
    let (mut inline, _) = engine(Mode::Inline);
    assert_eq!(inline.try_store(CMD_FIFO, 32, 0), Write::Busy, "an inline store runs in the call");
}

/// The store that takes the level up to the `0x6b2` threshold raises flag 25, which the lockless path must
/// leave to `write`; the store counted as before the engine's view changes nothing.
#[test]
fn try_store_leaves_the_high_water_crossing_to_write() {
    let (mut e, _) = engine(Mode::Threaded);
    let flag = 0x0200_0000u64;
    assert_eq!(e.write(CMD_FIFO, 64, 0x6b2u64 << 32 | 2), Write::Done);
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    assert_eq!(get(&mut e, 0x1014) & flag, 0);
    let seen = e.frame_generation(0);
    assert_eq!(e.try_store(CMD_FIFO, 64, 0), Write::Busy, "two words reach the threshold of two");
    assert_eq!(e.frame_generation(0), seen, "a refused store changes nothing");
    assert_eq!(get(&mut e, 0x1014) & flag, 0);
    assert_eq!(e.try_store(CMD_FIFO, 32, 0), Write::Done, "one word stays below it");
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
    assert_eq!(e.write(CMD_FIFO, 64, 0), Write::Done);
    assert_ne!(get(&mut e, 0x1014) & flag, 0, "write raises the flag");
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

/// `busy_ns` is the wall time the board's own threads spent running work: nothing before any, some after a
/// command ran on the board's thread, and nothing ever for an inline engine, whose work runs inside the calls.
#[test]
fn the_board_threads_busy_time_counts_their_work_and_not_a_calls() {
    let run = |mode| {
        let (mut e, _) = engine(mode);
        assert_eq!(BoardEngine::busy_ns(&e), 0, "nothing run yet");
        route_to_heart(&mut e);
        put(&mut e, HOST_WRITE_UPPER, 0);
        put(&mut e, HOST_WRITE_LOWER, 0x10000);
        e.push_words(CMD_FIFO, &[0x0001_0a5a, 5 | 1 << 15, 0xbeef]);
        assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
        let busy = BoardEngine::busy_ns(&e);
        assert!(busy >= BoardEngine::busy_ns(&e), "never goes back");
        busy
    };
    assert!(run(Mode::Threaded) > 0, "the board's thread ran the command");
    assert_eq!(run(Mode::Inline), 0, "an inline engine runs it inside the calls");
}

/// Composing the scan-out on the CPU path does not wait for the board: it returns while the raster
/// thread is held inside a write, which a shell calling it under the BQL could not outlast.
#[test]
fn the_cpu_scanout_does_not_wait_for_the_board() {
    let (mut e, ram) = engine(Mode::Threaded);
    put(&mut e, HOST_WRITE_UPPER, 0);
    put(&mut e, HOST_WRITE_LOWER, 0x10000);
    ram.hold_writes_at(0x10000 + 2 * 5);
    e.push_words(CMD_FIFO, &[0x0001_0a5a, 5 | 1 << 15, 0xbeef]);
    let t = std::time::Instant::now();
    while !ram.held.load(Ordering::SeqCst) {
        assert!(t.elapsed().as_secs() < 10, "the fence never reached its write");
        std::thread::yield_now();
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = e.scanout();
        tx.send(()).unwrap();
        e
    });
    let returned = rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok();
    ram.release();
    let _ = worker.join();
    assert!(returned, "scanout waited for the held board");
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
        assert_eq!(drain(&mut e), vec![Event::Irq], "a reset drops what waited and says the levels may have changed");
    }
}

/// A GPU unit's loss, on whatever thread it is reported, names the adapter in a `GpuLost` event,
/// faults the engine and wakes the shell at once, not at its next pump.
#[test]
fn a_gpu_loss_posts_gpu_lost_faults_the_engine_and_wakes_the_shell() {
    let woken = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let w = woken.clone();
    let loss = GpuLoss::new();
    let notify: Box<dyn Fn() + Send + Sync> = Box::new(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let mut e = VproEngine::with_gpu(config("v12", Mode::Threaded), None, Shared(Arc::new(Ram::default())), Some(notify), None, loss.clone()).unwrap();
    let lost = loss.callback();
    std::thread::spawn(move || lost("NVIDIA GeForce RTX 4090: wgpu device lost (Unknown): gone")).join().unwrap();
    assert!(woken.load(Ordering::SeqCst) >= 1, "the shell was told without a call into the engine");
    assert_eq!(e.write(CMD_FIFO, 32, 0), Write::Fault);
    let seen = drain(&mut e);
    assert!(seen.contains(&Event::GpuLost("NVIDIA GeForce RTX 4090: wgpu device lost (Unknown): gone".into())), "{seen:?}");
    assert_eq!(seen.iter().filter(|x| matches!(x, Event::DeviceFault(_))).count(), 1, "{seen:?}");
    assert!(e.fault().unwrap().contains("RTX 4090"));
}

/// A store of 0x200 or 0x16d to the reset control register resets the board from inside the write,
/// posts an Irq and leaves the engine running; 0, 1 and other values change nothing.
#[test]
fn a_reset_control_pulse_resets_the_board_and_leaves_the_engine_running() {
    const FLCTL_CONFIG: u32 = 0x1070;
    const RESET_CONTROL: u32 = 0x100c;
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode);
        let identity = get(&mut e, 0x4);
        put(&mut e, FLCTL_CONFIG, 0x12345);
        let reset = get(&mut e, FLCTL_CONFIG);
        assert_eq!(reset, 0x12345);
        drain(&mut e);
        for value in [0, 1, 0x100] {
            put(&mut e, RESET_CONTROL, value);
            assert_eq!(get(&mut e, FLCTL_CONFIG), 0x12345, "a store of {value:#x} does nothing");
        }
        assert!(drain(&mut e).is_empty(), "nor does it post anything");
        for value in [0x200, 0x16d] {
            put(&mut e, FLCTL_CONFIG, 0x12345);
            put(&mut e, RESET_CONTROL, value);
            assert_eq!(get(&mut e, FLCTL_CONFIG), 0x1c000, "{mode:?}: a store of {value:#x} resets the board");
            assert_eq!(get(&mut e, 0x4), identity, "its identity survives");
            assert_eq!(drain(&mut e), vec![Event::Irq], "levels may have changed");
            assert_eq!((e.fault(), e.write(CMD_FIFO, 32, 0)), (None, Write::Done), "and the engine runs on");
        }
        e.set_source(Source { id: 3, charged: true });
        put(&mut e, CMD_FIFO, 0);
        put(&mut e, RESET_CONTROL, 0x200);
        assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
        e.advance(0);
        assert!(drain(&mut e).contains(&Event::Credits { source: 3, retired: 1 }), "credits owed are not dropped");
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

/// The picture of the one head comes back by the trait's `scanout` (the inherent one has no head), and a GPU engine lends it where it is composed.
#[test]
fn the_head_s_picture_is_copied_and_a_gpu_engine_lends_it() {
    let (mut e, _) = engine(Mode::Inline);
    assert_eq!(e.heads(), 1);
    let mut px = Vec::new();
    assert_eq!(BoardEngine::scanout(&mut e, 0, &mut px), Ok(Some((1280, 1024))));
    assert_eq!(px.len(), 1280 * 1024);
    assert_eq!(BoardEngine::scanout(&mut e, 1, &mut px), Ok(None), "no second head");
    assert!(px.is_empty());
    assert!(e.lend_frame(0).unwrap().is_none(), "no GPU unit, nothing to lend");

    let mut cfg = config("v12", Mode::Inline);
    cfg.gpu = Some("auto".into());
    let Ok(mut g) = VproEngine::new(cfg, None, Shared(Arc::new(Ram::default())), None) else { return };
    // Until the registers describe a frame there is none to lend; one that is lent is the board's wgpu buffer.
    if let Some(lent) = g.lend_frame(0).unwrap() {
        assert_eq!((lent.width, lent.height), (1280, 1024));
        assert!(lent.buffer.downcast_ref::<wgpu::Buffer>().is_some());
    }
    assert_eq!(BoardEngine::scanout(&mut g, 0, &mut px), Ok(Some((1280, 1024))), "the copy still works beside the lend");
}

/// A shell's test of its busy-read path: the held slice keeps queued work pending, so a read that
/// waits answers busy until the hold ends.
#[test]
fn a_held_slice_makes_a_waiting_read_busy_until_it_is_released() {
    let (inline, _) = engine(Mode::Inline);
    assert!(!inline.hold_next_slice(10), "an inline board has no thread to hold");
    let (mut e, _) = engine(Mode::Threaded);
    assert!(e.hold_next_slice(60_000));
    put(&mut e, CMD_FIFO, 0);
    let start = std::time::Instant::now();
    while !e.slice_open() {
        assert!(start.elapsed().as_secs() < 20, "the held slice did not open");
        std::thread::yield_now();
    }
    assert!(e.access_waits(CMD_FIFO, 32, false));
    assert_eq!(e.read(CMD_FIFO, 32), Read::Busy);
    e.release_hold();
    while e.read(CMD_FIFO, 32) == Read::Busy {
        assert!(start.elapsed().as_secs() < 40, "the release did not end the hold");
        std::thread::yield_now();
    }
}

/// The frame generation moves with anything that can change the picture and stays put when nothing
/// does, so a shell that composed at one value can skip while it reads the same; compose_into has
/// nothing to compose into on an engine with no GPU unit, and refuses what is not a GPU buffer.
#[test]
fn the_frame_generation_moves_with_stores_and_holds_when_idle() {
    let (mut e, _) = engine(Mode::Inline);
    let g = BoardEngine::frame_generation(&e, 0);
    assert_ne!(g, 0);
    assert_eq!(BoardEngine::frame_generation(&e, 0), g, "nothing happened");
    assert_eq!(BoardEngine::frame_generation(&e, 1), 0, "no such head keeps none");
    let _ = e.read(0x4, 32);
    e.advance(1_000);
    assert_eq!(BoardEngine::frame_generation(&e, 0), g, "a read and a pump are not changes");
    put(&mut e, CMD_FIFO, 0);
    let g2 = BoardEngine::frame_generation(&e, 0);
    assert_ne!(g2, g, "a store");
    e.cpu_wrote(0, 8);
    assert_ne!(BoardEngine::frame_generation(&e, 0), g2, "a CPU write");
    assert_eq!(e.compose_into(0, &0u8, 1280), Ok(None), "no GPU unit, nothing to compose into");
}

/// A GPU engine's scan-out waits for the GPU and not for the board's queued CPU work: with the board's
/// thread held inside a slice, the screendump's frame still comes back at once (it took 1.3 s against 45 ms
/// on the CPU path when it drained the board first). Needs a GPU adapter, and returns early without one.
#[test]
fn a_gpu_scanout_does_not_wait_for_a_held_slice() {
    let mut cfg = config("v12", Mode::Threaded);
    cfg.gpu = Some("auto".into());
    let Ok(mut e) = VproEngine::new(cfg, None, Shared(Arc::new(Ram::default())), None) else { return };
    assert!(e.hold_next_slice(15_000));
    put(&mut e, CMD_FIFO, 0);
    let start = std::time::Instant::now();
    while !e.slice_open() {
        assert!(start.elapsed().as_secs() < 10, "the held slice did not open");
        std::thread::yield_now();
    }
    let asked = std::time::Instant::now();
    let frame = e.scanout();
    let took = asked.elapsed();
    e.release_hold();
    assert!(frame.is_some(), "a frame");
    assert!(took.as_secs_f64() < 7.0, "the scan-out waited {took:?} for a slice held 15 s");
}

/// A CPU store into lent memory, announced with `cpu_wrote`, is in the next scan-out of a GPU engine: the
/// call submits it to the unit before it composes, as it did before it stopped settling the board (the
/// regression: the frame stayed as the unit last drew it). Needs a GPU adapter; returns early without one.
#[test]
fn a_cpu_store_announced_with_cpu_wrote_reaches_a_gpu_scanout() {
    let bytes = memory_bytes("v12").unwrap() as usize;
    let mut buf = vec![0u64; bytes / 8];
    let lent = unsafe { Lent::new(buf.as_mut_ptr().cast(), bytes) }.unwrap();
    let mut cfg = config("v12", Mode::Inline);
    cfg.gpu = Some("auto".into());
    let Ok(mut e) = VproEngine::new(cfg, Some(lent), Shared(Arc::new(Ram::default())), None) else { return };
    let word = |e: &mut VproEngine, w: u32| assert_eq!(e.write(BACKEND_SELECT, 32, w as u64), Write::Done);
    let entry = |e: &mut VproEngine, index: u32, v: u32| {
        word(e, 0x3000_0000 | index << 14 | 1);
        word(e, v);
    };
    // What the X server programs: an RGB display mode for window id 0, the identity gamma ramp, and
    // the scan-out's buffers (a 1280 by 1024 frame of 16-byte pixels at 0x2000).
    entry(&mut e, DISPMODE_BASE, 0x0096_55a6);
    word(&mut e, 0x3000_0000 | 0x1a00 << 14 | 0x600);
    for i in 0..0x600u32 {
        let v = if i < 0x200 { i >> 2 } else if i < 0x400 { ((i - 0x200) >> 1) + 0x80 } else { (i - 0x300) + 0x100 };
        word(&mut e, v << 20 | v << 10 | v);
    }
    for (i, v) in [(0, 0x14_2000), (1, 0x2000), (2, 1280), (3, 1024), (7, 80), (9, 3)] {
        entry(&mut e, 0x2800 + i, v);
    }
    e.quiesce(u64::MAX).unwrap();
    e.cpu_wrote(0, bytes as u64);
    assert!(e.lend_frame(0).unwrap().is_some(), "the GPU scan-out is the one composing");
    let before = e.scanout().expect("a frame");
    assert_eq!((before.0, before.1), (1280, 1024));
    let at = 50 * 1280 + 100;
    assert_eq!(before.2[at], 0, "nothing at (100, 50) yet");


    // The CPU fills the frame buffer's 20 MB with ones straight in the lent memory, as the shell's CPU
    // access does, and announces it.
    let (from, len) = (0x2_0000usize, 1280 * 1024 * 16);
    buf[from / 8..(from + len) / 8].fill(u64::MAX);
    e.cpu_wrote(from as u64, len as u64);
    let after = e.scanout().expect("a frame");
    assert_ne!(after.2[at], 0, "the CPU's store is in the frame");
}

/// `scanout10` gives the colour maps' ten bits a channel in XB30 order (red low): its top eight bits are
/// `scanout`'s, its low two are the map entry's own (not a widened copy), and the CPU compose, the GPU scan-out
/// and `compose_into10` agree on every pixel. The head says 10 bits. The GPU half needs an adapter and is
/// skipped without one.
#[test]
fn scanout10_gives_the_ten_bit_colour_maps_on_the_cpu_and_the_gpu() {
    let bytes = memory_bytes("v12").unwrap() as usize;
    // A ramp over the 0x600 gamma entries, and the entry an 8-bit channel value selects (12 bits wide,
    // then the table's three ranges).
    let ramp = |t: u32| t * 0x3ff / 0x5ff;
    let entry_of = |c: u32| {
        let v = (c << 4) | (c >> 4);
        ramp(if v < 0x200 { v } else if v < 0x400 { v / 2 + 0x100 } else { v / 4 + 0x200 })
    };
    let mut frames = Vec::new();
    for gpu in [false, true] {
        let mut buf = vec![0u64; bytes / 8];
        let lent = unsafe { Lent::new(buf.as_mut_ptr().cast(), bytes) }.unwrap();
        let mut cfg = config("v12", Mode::Inline);
        cfg.gpu = gpu.then(|| "auto".into());
        let loss = GpuLoss::new();
        let adapter = if gpu { sgi_odyssey::wgpu::Gpu::new(&sgi_odyssey::wgpu::GpuOptions::default(), loss.callback()).ok() } else { None };
        let made = match &adapter {
            Some(g) => VproEngine::with_gpu(cfg, Some(lent), Shared(Arc::new(Ram::default())), None, Some(g.clone()), loss),
            None => VproEngine::new(cfg, Some(lent), Shared(Arc::new(Ram::default())), None),
        };
        // No adapter: the CPU half stands alone.
        let mut e = match made {
            Ok(e) => e,
            Err(why) => {
                assert!(gpu, "the CPU engine: {why:?}");
                assert!(std::env::var_os("IRIS_REQUIRE_GPU").is_none_or(|v| v != "1"), "no GPU: {why:?}");
                break;
            }
        };
        let word = |e: &mut VproEngine, w: u32| assert_eq!(e.write(BACKEND_SELECT, 32, w as u64), Write::Done);
        let entry = |e: &mut VproEngine, index: u32, v: u32| {
            word(e, 0x3000_0000 | index << 14 | 1);
            word(e, v);
        };
        entry(&mut e, DISPMODE_BASE, 0x0096_55a6);
        word(&mut e, 0x3000_0000 | 0x1a00 << 14 | 0x600);
        for i in 0..0x600u32 {
            // Each channel's ten bits differ from the others', and from an 8-bit value widened.
            let v = ramp(i);
            word(&mut e, (v ^ 1) << 20 | (v ^ 2) << 10 | (v ^ 3));
        }
        for (i, v) in [(0, 0x14_2000), (1, 0x2000), (2, 1280), (3, 1024), (7, 80), (9, 3)] {
            entry(&mut e, 0x2800 + i, v);
        }
        e.quiesce(u64::MAX).unwrap();
        let (from, len) = (0x2_0000usize, 1280 * 1024 * 16);
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for w in &mut buf[from / 8..(from + len) / 8] {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *w = x;
        }
        e.cpu_wrote(0, bytes as u64);
        let (mut eight, mut ten) = (Vec::new(), Vec::new());
        assert_eq!(BoardEngine::scanout(&mut e, 0, &mut eight), Ok(Some((1280, 1024))));
        assert_eq!(BoardEngine::scanout10(&mut e, 0, &mut ten), Ok(Some((1280, 1024))));
        assert_eq!(BoardEngine::scanout10(&mut e, 1, &mut Vec::new()), Ok(None), "no second head");
        let reachable = |c: u32| (0..256).any(|i| [entry_of(i) ^ 1, entry_of(i) ^ 2, entry_of(i) ^ 3].contains(&c));
        let widened = |c: u32| (c << 2) | (c >> 6);
        let mut own_low_bits = 0;
        for (&a, &t) in eight.iter().zip(&ten) {
            let top = |c: u32| (c >> 2) & 0xff;
            assert_eq!(a, top(t) << 16 | top(t >> 10) << 8 | top(t >> 20), "the top eight bits are scanout's, red low");
            assert_eq!(t >> 30, 0);
            assert!([t, t >> 10, t >> 20].iter().all(|&c| reachable(c & 0x3ff)), "a channel is a map entry's");
            own_low_bits += [t, t >> 10, t >> 20].iter().any(|&c| c & 0x3ff != widened((c >> 2) & 0xff)) as u32;
        }
        assert!(own_low_bits > 0, "the low two bits are the maps' own");
        assert_eq!((BoardEngine::head_bits(&e, 0), BoardEngine::head_bits(&e, 1)), (10, 8));
        if let Some(g) = &adapter {
            // The wide compose into the caller's buffer, with a stride past the width, is scanout10 word for word.
            let (stride, usage) = (1300u32, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);
            let dst = g.buffer("wide", stride as u64 * 1024 * 4, usage);
            assert_eq!(BoardEngine::compose_into10(&mut e, 0, &dst, stride), Ok(Some((1280, 1024))));
            let mut got = vec![0u32; stride as usize * 1024];
            g.read(&dst, &[(0, got.len() as u64 * 4)], bytemuck::cast_slice_mut(&mut got)).unwrap();
            for y in 0..1024usize {
                assert!(got[y * stride as usize..y * stride as usize + 1280] == ten[y * 1280..(y + 1) * 1280], "row {y} of compose_into10 differs from scanout10");
            }
            assert_eq!(BoardEngine::compose_into10(&mut e, 1, &dst, stride), Ok(None), "no second head");
        }
        frames.push(ten);
    }
    if let [cpu, gpu] = &frames[..] {
        assert!(cpu == gpu, "the GPU scan-out is the CPU compose");
    }
}

/// A shell sizes its display buffer from `frame_size`, and a destination too small for the frame is an error of
/// the call, not a fault: the GPU unit and the engine go on (it stopped the machine before). Needs an adapter.
#[test]
fn a_destination_too_small_for_the_frame_is_an_error_and_not_a_fault() {
    use sgi_odyssey::wgpu::{Gpu, GpuOptions};
    let (cpu, _) = engine(Mode::Inline);
    assert_eq!((BoardEngine::frame_size(&cpu, 0), BoardEngine::frame_size(&cpu, 1)), (Some((1280, 1024)), None));
    let loss = GpuLoss::new();
    let Ok(gpu) = Gpu::new(&GpuOptions::default(), loss.callback()) else { return };
    let mut e = VproEngine::with_gpu(config("v12", Mode::Inline), None, Shared(Arc::new(Ram::default())), None, Some(gpu.clone()), loss).unwrap();
    let usage = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC;
    for (what, dst, stride) in [("too small", gpu.buffer("small", 4096, usage), 1280), ("a stride under the width", gpu.buffer("narrow", 1280 * 1024 * 4, usage), 1000), ("no storage usage", gpu.buffer("no storage", 1280 * 1024 * 4, wgpu::BufferUsages::COPY_SRC), 1280)] {
        let r = BoardEngine::compose_into(&mut e, 0, &dst, stride);
        assert!(matches!(r, Err(Fault(ref why)) if why.starts_with("compose_into:")), "{what}: {r:?}");
        assert_eq!(e.fault(), None, "{what}: the engine goes on");
    }
    let fits = gpu.buffer("fits", 1280 * 1024 * 4, usage);
    assert!(BoardEngine::compose_into(&mut e, 0, &fits, 1280).is_ok(), "and takes a destination that fits");
    assert_eq!(e.fault(), None);
}

/// The scan-out's timing comes out of the engine for a shell that paces its own retrace by the board's: the
/// powered board's 1280 by 1024 mode (the model's own default timing, 1688 by 1066 clocks at 107,964,476 Hz),
/// one field, the shell's clock starting the phase until a write anchors it, a generation that moves with a
/// new mode. A second head has none.
#[test]
fn head_zero_reports_the_scanout_timing_and_a_generation() {
    let (e, _) = engine(Mode::Inline);
    let t = BoardEngine::head_timing(&e, 0).expect("the powered board has a mode");
    assert_eq!((t.frame_clocks, t.clock_hz, t.fields, t.lines, t.epoch_ns), (1688 * 1066, 107_964_476, 1, 1066, -1));
    assert_ne!(t.generation, 0);
    assert_eq!(BoardEngine::head_timing(&e, 1), None);
}

/// A write of a window's bit to the swap-arm register (0x10b4) is the guest's swap request: one `Event::Swap` each, counting
/// from 1, with the shell time the engine had, and two with no retrace between them are two events (the arm bits OR together
/// in the register, the events do not).
#[test]
fn every_swap_arm_write_is_its_own_event_with_a_running_count() {
    let (mut e, _) = engine(Mode::Inline);
    let swaps = |e: &mut VproEngine| {
        let mut v = Vec::new();
        BoardEngine::drain_events(e, &mut v, usize::MAX);
        v.into_iter().filter_map(|x| if let Event::Swap { count, time_ns } = x { Some((count, time_ns)) } else { None }).collect::<Vec<_>>()
    };
    BoardEngine::advance(&mut e, 500);
    assert_eq!(BoardEngine::write(&mut e, 0x10b4, 32, 1), Write::Done);
    assert_eq!(BoardEngine::write(&mut e, 0x10b4, 32, 1), Write::Done);
    assert_eq!(BoardEngine::write_at(&mut e, 900, 0x10b4, 32, 2), Write::Done);
    assert_eq!(BoardEngine::write(&mut e, 0x10b4, 32, 0), Write::Done, "an empty arm is no swap");
    assert_eq!(swaps(&mut e), vec![(1, 500), (2, 500), (3, 900)]);
    assert_eq!(BoardEngine::swaps_dropped(&e), 0);
}
