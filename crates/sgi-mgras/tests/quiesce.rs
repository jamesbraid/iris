//! The call contract: writes queue and answer `Busy` when the queue is full,
//! reads answer `Busy` while queued work could change them, status values
//! never wait, the pump never runs work, and quiescence drains and parks.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::hq3::host;
use sgi_engine::{BoardEngine, Event, Idle, Read, Refused, Write};
use support::*;

fn engine(mode: Mode, queue_ops: usize, notes: Option<Arc<AtomicU32>>) -> (MgrasEngine<Mem>, Mem) {
    let mem = Mem::default();
    let mut cfg = Config::new(Board::High, mode);
    cfg.queue_ops = queue_ops;
    let notify = notes.map(|n| Box::new(move || drop(n.fetch_add(1, Ordering::SeqCst))) as Box<dyn Fn() + Send + Sync>);
    (MgrasEngine::new(cfg, mem.clone(), None, notify).unwrap(), mem)
}

/// A raster register write that changes nothing the status shows.
const REG: u32 = host::RASTER + 4 * 0x10;

#[test]
fn a_full_queue_refuses_a_write_and_a_run_makes_room() {
    let (mut e, _) = engine(Mode::Inline, 8, None);
    for i in 0..8 {
        assert_eq!(e.write(REG, 32, i), Write::Done, "write {i}");
    }
    assert_eq!(e.write(REG, 32, 99), Write::Busy);
    assert_eq!(e.push_words(0, &[0; 4]), 0, "a batch takes what fits, here nothing");
    assert_eq!(e.run(usize::MAX), 8);
    assert_eq!(e.write(REG, 32, 9), Write::Done);
    assert_eq!(e.push_words(0, &[0; 20]), 7, "a batch takes what fits");
}

#[test]
fn status_never_waits_and_reads_wait_for_queued_work() {
    let (mut e, _) = engine(Mode::Inline, 1024, None);
    let idle = host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE;
    assert_eq!(e.read(host::STATUS, 32), Read::Value((idle | host::STATUS_FIXED | host::STATUS_CFIFO_LOW) as u64));
    e.write(REG, 32, 0x1234);
    e.push_words(0, &[0; 5]);
    // Queued: status answers at once, not idle, with the FIFO level.
    let Read::Value(s) = e.read(host::STATUS, 32) else { panic!("status waited") };
    assert_eq!(s as u32 & idle, 0);
    assert_eq!(e.read(host::FIFOSTATUS, 32), Read::Value(5));
    // A register read depends on the queue.
    assert_eq!(e.read(REG, 32), Read::Busy);
    // The pump hands work over and never runs it.
    let p = e.advance(1_000);
    assert_eq!(p.idle, Idle::Working);
    assert_eq!(e.read(REG, 32), Read::Busy);
    assert_eq!(e.now(), 1_000);
    e.run(usize::MAX);
    assert_eq!(e.advance(2_000).idle, Idle::Yes);
    assert_eq!(e.read(REG, 32), Read::Value(0x1234));
    assert_eq!(e.read(host::FIFOSTATUS, 32), Read::Value(0));
}

#[test]
fn the_notify_hook_only_says_that_something_waits() {
    let n = Arc::new(AtomicU32::new(0));
    let (mut e, _) = engine(Mode::Inline, 1024, Some(n.clone()));
    for _ in 0..10 {
        e.write(REG, 32, 1);
    }
    assert_eq!(n.load(Ordering::SeqCst), 1, "once for a batch of waiting work");
    e.advance(0);
    e.write(REG, 32, 1);
    assert_eq!(n.load(Ordering::SeqCst), 2, "again after the pump");
    e.run(usize::MAX);
    e.write(host::SET_FLAGS, 32, 1 << 20);
    let mut ev = Vec::new();
    e.drain_events(&mut ev, usize::MAX);
    assert_eq!(ev, vec![Event::Irq]);
}

/// A worker that finishes a batch tells the shell, so a pump that went idle on a quiet queue runs again.
#[test]
fn a_worker_finishing_a_batch_calls_the_notify_hook() {
    let n = Arc::new(AtomicU32::new(0));
    let (mut e, _) = engine(Mode::Threaded, 1024, Some(n.clone()));
    // Parked, the write waits for the worker, so the rearm comes before the batch runs.
    e.quiesce(u64::MAX).unwrap();
    e.write(REG, 32, 1);
    assert_eq!(n.load(Ordering::SeqCst), 1, "the queued write");
    e.advance(0);
    e.resume();
    assert_eq!(e.wait_idle(0, 10_000_000_000), Idle::Yes);
    // Idle can be seen just before the worker's wake: wait for the hook itself.
    let began = std::time::Instant::now();
    while n.load(Ordering::SeqCst) < 2 && began.elapsed() < std::time::Duration::from_secs(30) {
        std::thread::yield_now();
    }
    assert_eq!(n.load(Ordering::SeqCst), 2, "the finished batch");
}

#[test]
fn a_worker_runs_the_queue_and_quiescence_parks_it() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-swap-tmesh-sphere").unwrap().1);
    let mut p = Player::new(config(&rep.head, Mode::Threaded, None), Drive::Lazy);
    let main = std::thread::current().id();
    rep.cmds.iter().for_each(|c| p.apply(c));
    // Running means saving is refused.
    assert_eq!(p.e.save(&mut Vec::new()), Err(Refused::NotQuiesced));
    assert_eq!(p.e.observe_all(), Err(Refused::NotQuiesced));
    p.e.quiesce(u64::MAX).unwrap();
    assert_eq!(p.e.advance(0).idle, Idle::Yes, "quiescence drained the queue");
    let hash = p.e.state_hash().unwrap();
    p.e.observe_all().unwrap();
    p.e.save(&mut Vec::new()).unwrap();
    // The worker, not this thread, used host memory, and only the RAM path.
    let threads = p.mem.0.ram_threads.lock().clone();
    assert!(threads.len() <= 1 && !threads.contains(&main), "{threads:?}");
    assert_eq!(p.mem.0.bus_calls.load(Ordering::Relaxed), 0);

    // Parked, a write waits in the queue; a resume runs it.
    p.e.write(REG, 32, 0x77);
    assert_eq!(p.e.read(REG, 32), Read::Busy);
    p.e.resume();
    assert_eq!(p.e.wait_idle(0, 10_000_000_000), Idle::Yes);
    assert_eq!(p.e.read(REG, 32), Read::Value(0x77));
    assert_eq!(p.e.state_hash().unwrap(), hash);
}

#[test]
fn reset_returns_to_power_on() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-swap-tmesh-sphere").unwrap().1);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    let empty = p.e.state_hash().unwrap();
    rep.cmds.iter().for_each(|c| p.apply(c));
    assert_ne!(p.e.state_hash().unwrap(), empty);
    p.e.quiesce(u64::MAX).unwrap();
    p.e.reset().unwrap();
    assert_eq!(p.e.state_hash().unwrap(), empty);
    assert_eq!(p.e.flags(), 0);
    let mut ev = Vec::new();
    p.e.drain_events(&mut ev, usize::MAX);
    assert_eq!(ev, vec![Event::Irq], "a reset announces the levels it cleared");
}

/// DMA that reaches no RAM is reported, not guessed.
#[test]
fn dma_outside_ram_is_a_bus_fault_event() {
    let rep = parse(REPLAYS.iter().find(|r| r.0 == "gl-eviction-i3dm").unwrap().1);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    p.mem.0.fail_writes.store(true, Ordering::Relaxed);
    rep.cmds.iter().for_each(|c| p.apply(c));
    drive(&mut p.e);
    p.finish();
    assert!(p.bus_faults.iter().any(|f| f.1), "{:?}", p.bus_faults);
}

/// A shell can ask before it calls.
#[test]
fn access_waits_says_what_a_call_would_do() {
    let (mut e, _) = engine(Mode::Inline, 4, None);
    assert!(!e.access_waits(REG, 32, true));
    assert!(!e.access_waits(REG, 32, false), "idle: a read answers");
    for i in 0..4 {
        assert_eq!(e.write(REG, 32, i), Write::Done);
    }
    assert!(e.access_waits(REG, 32, true), "the queue is full");
    assert!(!e.access_waits(host::SET_FLAGS, 32, true), "flags are not queued");
    assert!(e.access_waits(REG, 32, false), "a read waits for queued work");
    assert!(!e.access_waits(host::STATUS, 32, false), "status never waits");
    e.run(usize::MAX);
    assert!(!e.access_waits(REG, 32, true) && !e.access_waits(REG, 32, false));
}

/// The interrupt line is the engine's flags under its interrupt enable, read
/// without waiting, and every change that can move it is announced.
#[test]
fn the_interrupt_level_follows_the_flags_and_the_enable() {
    let (mut e, _) = engine(Mode::Inline, 1024, None);
    let drain = |e: &mut MgrasEngine<Mem>| {
        let mut ev = Vec::new();
        e.drain_events(&mut ev, usize::MAX);
        ev
    };
    assert_eq!(e.irq_levels(), 0);
    e.write(host::SET_FLAGS, 32, 1 << 16);
    assert_eq!((e.irq_levels(), drain(&mut e)), (0, vec![Event::Irq]), "a flag nobody enabled");
    e.write(host::INTERRUPT_ENABLE_SET, 32, 1 << 16);
    assert_eq!((e.irq_levels(), drain(&mut e)), (1, vec![Event::Irq]), "the enable raised it");
    assert_eq!(e.advance(0).irq_levels, 1);
    e.write(host::CLEAR_FLAGS, 32, 1 << 16);
    assert_eq!((e.irq_levels(), drain(&mut e)), (0, vec![Event::Irq]));
    // A saved level comes back with a load, which announces it.
    e.write(host::SET_FLAGS, 32, 1 << 16);
    e.quiesce(u64::MAX).unwrap();
    let mut state = Vec::new();
    e.save(&mut state).unwrap();
    e.reset().unwrap();
    assert_eq!((e.irq_levels(), drain(&mut e)), (0, vec![Event::Irq]), "a reset announces the level it cleared");
    e.load(&mut sgi_engine::VecSource::new(state)).unwrap();
    assert_eq!(e.irq_levels(), 1);
    assert!(drain(&mut e).contains(&Event::Irq));
}

/// A guest that polls status with a part-full stage must see its work run: the
/// poll hands the stage to the worker, with no pump.
#[test]
fn a_status_poll_hands_staged_work_to_the_worker() {
    let (mut e, _) = engine(Mode::Threaded, 1024, None);
    let idle = host::STATUS_HQ_IDLE | host::STATUS_BOARD_IDLE;
    e.write(REG, 32, 0x55);
    let start = std::time::Instant::now();
    loop {
        let Read::Value(s) = e.read(host::STATUS, 32) else { panic!("status waited") };
        if s as u32 & idle == idle {
            break;
        }
        assert!(start.elapsed().as_secs() < 30, "the staged write never ran");
        std::thread::yield_now();
    }
    assert_eq!(e.read(REG, 32), Read::Value(0x55));
}

/// A quiesce with a budget answers `Midway` for work it could not finish and
/// goes on running; with room it parks.
#[test]
fn quiesce_with_a_budget_answers_midway_instead_of_running_on() {
    let (mut e, _) = engine(Mode::Inline, 20_000, None);
    for i in 0..10_000 {
        assert_eq!(e.write(REG, 32, i), Write::Done);
    }
    let Err(Refused::Midway(why)) = e.quiesce(0) else { panic!("no budget, yet it finished 10,000 writes") };
    assert!(why.contains("budget"), "{why}");
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::NotQuiesced), "it did not park");
    e.quiesce(u64::MAX).unwrap();
    assert_eq!(e.save(&mut Vec::new()), Ok(()));
    assert_eq!(e.wait_idle(0, 0), Idle::Yes, "MGRAS never stalls on the shell");
}

/// The Digital Video driver starts its video DMA with bits 0 and 4 of the word at HQ4 `+0x081010` and polls bit 12 of the
/// status (`+0x020000`) up to 200 times for it to clear (gr4 video-sync.md section 3.4, software expectation); a store of 2
/// abandons the transfer. The transfer's data path is not modelled, so busy lasts one status poll.
#[test]
fn the_video_dma_busy_bit_shows_after_the_start_store_and_clears_within_the_drivers_polls() {
    let (mut e, _) = engine(Mode::Inline, 1024, None);
    let busy = |e: &mut MgrasEngine<Mem>| matches!(e.read(host::STATUS, 32), Read::Value(v) if v & 0x1000 != 0);
    assert!(!busy(&mut e), "idle");
    assert_eq!(e.write(0x81010, 32, 0x1), Write::Done);
    assert!(!busy(&mut e), "bit 0 alone does not start it");
    assert_eq!(e.write(0x81010, 32, 0x11), Write::Done);
    assert!(busy(&mut e), "started");
    let polls = (0..200).take_while(|_| busy(&mut e)).count();
    assert!(polls < 200, "it clears within the driver's 200 polls");
    assert!(!busy(&mut e));
    assert_eq!(e.write(0x81010, 32, 0x11), Write::Done);
    assert_eq!(e.write(0x81010, 32, 0x2), Write::Done);
    assert!(!busy(&mut e), "the abort clears it");
    assert_eq!(e.write(0x81010, 32, 0x11), Write::Done);
    e.quiesce(u64::MAX).unwrap();
    e.reset().unwrap();
    assert!(!busy(&mut e), "a reset clears it");
}

/// Each CFIFO swap token (0x37) is its own `Event::Swap`, counting from 1, with the shell time the engine had; the FLAG_CP0
/// flag the kernel acknowledges is one bit, so two swaps with no acknowledge between them still give two events. Inline and
/// on the worker.
#[test]
fn every_swap_token_is_its_own_event_even_with_no_acknowledge_between() {
    for mode in [Mode::Inline, Mode::Threaded] {
        let (mut e, _) = engine(mode, 1024, None);
        e.advance(1_000);
        for _ in 0..3 {
            assert_eq!(e.write(host::CFIFO, 32, 0x37 << 8), Write::Done);
        }
        assert_eq!(e.wait_idle(2_000, 10_000_000_000), Idle::Yes, "{mode:?}");
        e.run(usize::MAX);
        let mut ev = Vec::new();
        e.drain_events(&mut ev, usize::MAX);
        let swaps: Vec<(u64, u64)> = ev.iter().filter_map(|x| if let Event::Swap { count, time_ns } = x { Some((*count, *time_ns)) } else { None }).collect();
        assert_eq!(swaps.iter().map(|s| s.0).collect::<Vec<_>>(), [1, 2, 3], "{mode:?}");
        assert!(swaps.iter().all(|s| (1_000..=2_000).contains(&s.1)), "{mode:?}: {swaps:?}");
        assert_eq!(e.read(host::SET_FLAGS, 32), Read::Value(0x400), "{mode:?}: one flag bit for all three");
        assert_eq!(e.swaps_dropped(), 0);
    }
}
