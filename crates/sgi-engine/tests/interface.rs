//! Every part of the trait, through the mock board: interrupts, credits,
//! lent memory and dirty runs, `frame`, stalls and `Midway`, refusals, the
//! save stream, faults and the shell loops.

mod support;

use std::sync::Arc;
use sgi_engine::mock::{reg, MockConfig, STATE_VERSION};
use sgi_engine::shell::{drain_stalls, quiesce};
use sgi_engine::*;
use support::*;

fn drain(e: &mut impl BoardEngine<Region = (u32, u32)>) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, usize::MAX);
    v
}

fn sum(e: &mut impl BoardEngine<Region = (u32, u32)>) -> Read {
    e.read(reg::SUM, 32)
}

#[test]
fn interrupt_levels_come_from_atomics_and_changes_are_announced_once() {
    let (mut e, _) = mock(MockConfig::default());
    assert_eq!(e.irq_levels(), 0);
    for bit in [1, 2, 4] {
        assert_eq!(e.write(reg::IRQ_SET, 32, bit), Write::Done);
    }
    assert_eq!(e.irq_levels(), 7);
    assert_eq!(drain(&mut e), vec![Event::Irq], "three changes, one event until drained");
    e.write(reg::IRQ_CLR, 32, 2);
    assert_eq!(e.advance(0).irq_levels, 5, "the pump carries the levels");
    assert_eq!(drain(&mut e), vec![Event::Irq]);
    // A reset and a load change the levels without a write: both announce it.
    e.quiesce(u64::MAX).unwrap();
    e.reset().unwrap();
    assert_eq!((e.irq_levels(), drain(&mut e)), (0, vec![Event::Irq]));
    e.write(reg::IRQ_SET, 32, 8);
    let mut state = Vec::new();
    e.save(&mut state).unwrap();
    e.reset().unwrap();
    drain(&mut e);
    e.load(&mut VecSource::new(state)).unwrap();
    assert_eq!((e.irq_levels(), drain(&mut e)), (8, vec![Event::Irq]));
}

#[test]
fn packets_keep_their_order_and_widget() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::PACKET, 32, 5);
    e.write(reg::PACKET, 32, 6);
    e.run(usize::MAX);
    let v: Vec<u8> = drain(&mut e).iter().map(|x| match x {
        Event::Interrupt(i) => {
            assert_eq!((i.widget, i.level), (8, true));
            i.vector
        }
        other => panic!("{other:?}"),
    }).collect();
    assert_eq!(v, [5, 6]);
}

#[test]
fn lines_that_reach_one_receiver_bit_are_found() {
    let line = |sender, addr, vector| Interrupt { sender, widget: 8, addr, vector, level: true };
    let by_vector = |i: &Interrupt| i.vector as u64;
    let distinct = [line(1, 0x10, 5), line(1, 0x10, 5), line(1, 0x10, 6), line(2, 0x10, 7)];
    assert_eq!(outbox::aliasing_lines(&distinct, by_vector), None);
    let alias = [line(1, 0x10, 5), line(2, 0x10, 5)];
    assert_eq!(outbox::aliasing_lines(&alias, by_vector), Some((alias[0], alias[1])));
    assert_eq!(outbox::aliasing_lines(&alias, |i| i.sender as u64), None, "a receiver that decodes the sender tells them apart");
}

#[test]
fn a_gpu_loss_reaches_the_handler_whether_it_came_before_or_after_attach() {
    use std::sync::Mutex;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (loss, sink) = (GpuLoss::new(), seen.clone());
    let lost = loss.callback();
    lost("RTX 4090: wgpu device lost (Unknown): first");
    lost("a second report before the engine");
    loss.attach(move |why| sink.lock().unwrap().push(why.to_string()));
    assert_eq!(*seen.lock().unwrap(), ["RTX 4090: wgpu device lost (Unknown): first"], "the first loss, once, at attach");
    lost("RTX 4090: wgpu device lost (Destroyed): later");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[test]
fn credits_go_to_the_source_of_the_store_and_coalesce() {
    let (mut e, _) = mock(MockConfig { queue_ops: 4096, ..Default::default() });
    e.set_source(Source { id: 7, charged: true });
    e.push_words(0, &[1; 1000]);
    e.set_source(Source { id: 9, charged: true });
    e.write(reg::DATA, 32, 1);
    e.set_source(Source { id: 11, charged: false });
    e.write(reg::DATA, 32, 1);
    e.run(usize::MAX);
    let ev = drain(&mut e);
    assert_eq!(ev, vec![Event::Credits { source: 7, retired: 1000 }, Event::Credits { source: 9, retired: 1 }], "a store that took no credit returns none");
    assert_eq!(sum(&mut e), Read::Value(1002));
}

#[test]
fn a_full_queue_is_busy_and_a_run_makes_room() {
    let (mut e, _) = mock(MockConfig { queue_ops: 4, ..Default::default() });
    assert_eq!(e.push_words(0, &[1; 6]), 4);
    assert_eq!(e.write(reg::DATA, 32, 1), Write::Busy);
    assert!(e.access_waits(reg::DATA, 32, true));
    assert!(!e.access_waits(reg::IRQ_SET, 32, true), "an immediate register never waits");
    assert_eq!(sum(&mut e), Read::Busy);
    assert!(e.access_waits(reg::SUM, 32, false));
    assert_eq!(e.write(reg::DATA, 16, 1), Write::Fault, "a width the board does not have");
    e.run(usize::MAX);
    assert_eq!(e.write(reg::DATA, 32, 1), Write::Done);
}

#[test]
fn a_swap_stalls_the_stream_until_the_frame() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::DATA, 32, 1);
    e.write(reg::SWAP, 32, 0);
    e.write(reg::DATA, 32, 10);
    e.write(reg::DATA, 32, 100);
    assert_eq!(e.wait_idle(0, 1_000), Idle::Stalled(StallOn::Frame), "no hang: the engine says what it waits for");
    assert_eq!(sum(&mut e), Read::Busy, "draws after the swap wait");
    assert_eq!(e.advance(5).idle, Idle::Stalled(StallOn::Frame));
    e.frame(10);
    assert_eq!(e.wait_idle(10, 1_000), Idle::Yes);
    assert_eq!(sum(&mut e), Read::Value(111));
}

#[test]
fn a_time_stall_ends_at_the_pump_and_names_its_deadline() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::WAIT, 32, 1_000);
    e.write(reg::DATA, 32, 3);
    assert_eq!(e.wait_idle(0, 0), Idle::Stalled(StallOn::Time(1_000)));
    let p = e.advance(999);
    assert_eq!((p.idle, p.next_deadline), (Idle::Stalled(StallOn::Time(1_000)), Some(1_000)));
    assert_eq!(e.advance(1_000).idle, Idle::Working, "released, not yet run");
    assert_eq!(e.wait_idle(1_000, 0), Idle::Yes);
    assert_eq!(sum(&mut e), Read::Value(3));
}

#[test]
fn quiesce_answers_midway_instead_of_hanging_and_the_shell_loop_finishes_it() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::SWAP, 32, 0);
    e.write(reg::WAIT, 32, 500);
    e.write(reg::DATA, 32, 4);
    let Err(Refused::Midway(why)) = e.quiesce(1_000) else { panic!("a stalled engine quiesced") };
    assert!(why.contains("frame"), "{why}");
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::NotQuiesced), "it was not parked");
    let mut now = 0;
    quiesce(&mut e, &mut now, 1_000, 8).unwrap();
    assert_eq!((now, e.frames), (500, 1), "the loop gave a frame, then the time");
    assert_eq!(e.save(&mut Vec::new()), Ok(()));
    e.resume();
    assert_eq!(sum(&mut e), Read::Value(4));
}

#[test]
fn the_shell_loop_gives_up_when_the_engine_never_finishes() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::SWAP, 32, 0);
    e.write(reg::DATA, 32, 1);
    // A loop that cannot release a stall (here: no rounds) reports it.
    assert_eq!(drain_stalls(&mut e, &mut 0, 0, 0), Idle::Working);
    assert!(matches!(quiesce(&mut e, &mut 0, 0, 0), Err(Refused::Midway(_))));
    // A read that must not fail runs the loop and then reads.
    assert_eq!(sum(&mut e), Read::Busy);
    assert_eq!(drain_stalls(&mut e, &mut 0, 0, usize::MAX), Idle::Yes);
    assert_eq!(sum(&mut e), Read::Value(1));
}

#[test]
fn a_fence_raises_its_interrupt_only_after_its_write_back() {
    let (mut e, ram) = mock(MockConfig::default());
    e.write(reg::DMA_DATA, 32, 0xFEED);
    e.write(reg::FENCE, 32, 0x100);
    e.run(usize::MAX);
    assert!(drain(&mut e).is_empty(), "the worker raises nothing");
    assert_eq!(ram.word(0x100), 0);
    e.advance(1);
    assert_eq!(ram.word(0x100), 0xFEED);
    let ev = drain(&mut e);
    assert!(matches!(ev.as_slice(), [Event::Interrupt(i)] if i.vector == 3), "{ev:?}");
}

#[test]
fn dma_that_reaches_no_ram_is_reported_once_a_page() {
    let (mut e, ram) = mock(MockConfig::default());
    e.write(reg::DMA_DATA, 32, 1);
    for a in [RAM, RAM + 8, RAM + 0x2000] {
        e.write(reg::DMA_WRITE, 32, a);
    }
    e.write(reg::DMA_READ, 32, RAM);
    e.write(reg::DMA_WRITE, 32, 0x40);
    e.run(usize::MAX);
    assert_eq!(ram.word(0x40), 1);
    let ev = drain(&mut e);
    assert_eq!(ev, vec![Event::BusFault { addr: RAM, write: true }, Event::BusFault { addr: RAM + 0x2000, write: true }, Event::BusFault { addr: RAM, write: false }]);
}

#[test]
fn lent_memory_untrapped_is_written_by_the_engine_and_reported_as_runs() {
    let mut buf = LentBuf::new();
    let lent = buf.lent();
    let (mut e, _) = mock(MockConfig { lent: Some(lent), ..Default::default() });
    let mut runs = Vec::new();
    e.trapped(&mut runs);
    assert!(runs.is_empty(), "no GPU unit, nothing trapped");
    // Words 1, 3000 and 7000 are in blocks 0, 1 and 3 of the 4 blocks.
    for w in [1u32, 3000, 7000] {
        e.write(reg::LENT_STORE + w * 8, 64, 0xAB00 + w as u64);
    }
    // A store the CPU could see before the engine ran is not there yet, and the
    // dirty list is read after the quiesce so the last writes are in it.
    assert_eq!(lent.load(1), 0);
    quiesce(&mut e, &mut 0, 0, 1).unwrap();
    e.drain_dirty(&mut runs);
    assert_eq!((lent.load(1), lent.load(3000), lent.load(7000)), (0xAB01, 0xAB00 + 3000, 0xAB00 + 7000));
    let b = DIRTY_BLOCK;
    assert_eq!(runs, vec![DirtyRun { offset: 0, len: 2 * b }, DirtyRun { offset: 3 * b, len: b }]);
    let mut again = Vec::new();
    e.drain_dirty(&mut again);
    assert!(again.is_empty(), "drained once");
}

#[test]
fn trapped_lent_memory_is_current_after_observe_and_takes_the_cpu_s_words_on_cpu_wrote() {
    let mut buf = LentBuf::new();
    let lent = buf.lent();
    let (mut e, _) = mock(MockConfig { lent: Some(lent), gpu_unit: true, ..Default::default() });
    let mut t = Vec::new();
    e.trapped(&mut t);
    assert_eq!(t, vec![DirtyRun { offset: 0, len: 32 * 1024 }]);
    // The CPU stores a word, tells the engine, which adds to it on the GPU's copy.
    lent.store(5, 100);
    e.cpu_wrote(40, 8);
    e.write(reg::LENT_ADD + 5 * 8, 64, 23);
    e.write(reg::LENT_STORE + 6 * 8, 64, 7);
    assert_eq!(lent.load(5), 100, "the GPU copy is newer, the CPU sees its own until it observes");
    e.observe_lent(40, 16).unwrap();
    assert_eq!((lent.load(5), lent.load(6)), (123, 7));
    let mut d = Vec::new();
    e.drain_dirty(&mut d);
    assert_eq!(d, vec![DirtyRun { offset: 0, len: DIRTY_BLOCK }], "what observe copied is dirty for the shell");
    // Untrapped words (second half) go straight to the lent memory.
    e.write(reg::LENT_STORE + 5000 * 8, 64, 9);
    e.run(usize::MAX);
    assert_eq!(lent.load(5000), 9);
    // Without cpu_wrote the engine would have added to its own stale word.
    lent.store(7, 1000);
    e.write(reg::LENT_ADD + 7 * 8, 64, 1);
    e.observe_lent(56, 8).unwrap();
    assert_eq!(lent.load(7), 1, "the engine never saw the CPU's word");
}

#[test]
fn every_refusal_has_its_own_reason() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::DATA, 32, 1);
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::NotQuiesced));
    assert_eq!(e.observe_all(), Err(Refused::NotQuiesced));
    assert_eq!(e.reset(), Err(Refused::NotQuiesced), "work is queued");
    e.quiesce(u64::MAX).unwrap();
    e.observe_all().unwrap();
    let mut good = Vec::new();
    e.save(&mut good).unwrap();
    assert_eq!(good[0].name, "meta", "meta first");

    let mismatch = |e: &mut dyn BoardEngine<Region = (u32, u32)>, c: Vec<Chunk>| matches!(e.load(&mut VecSource::new(c)), Err(Refused::Mismatch(_)));
    let mut old = good.clone();
    old[0].bytes[0] ^= 1;
    assert!(mismatch(&mut e, old), "another version");
    let mut bad = good.clone();
    bad.insert(0, Chunk { name: "shadow".into(), index: 0, bytes: vec![] });
    assert!(mismatch(&mut e, bad), "meta is not first");
    let mut bad = good.clone();
    bad.push(Chunk { name: "other".into(), index: 0, bytes: vec![] });
    assert!(mismatch(&mut e, bad), "a part nobody knows");
    assert!(mismatch(&mut e, vec![]), "an empty state");
    e.reset().unwrap();
    e.write(reg::SWAP, 32, 0);
    e.run(usize::MAX);
    e.quiesce(u64::MAX).unwrap();
    assert!(matches!(e.save(&mut Vec::new()), Err(Refused::Midway(_))), "a swap is held");
    e.fail("broken");
    assert_eq!(e.save(&mut Vec::new()), Err(Refused::Faulted("broken".into())));
    assert_eq!(e.load(&mut VecSource::new(good)), Err(Refused::Faulted("broken".into())));
}

#[test]
fn state_streams_in_pieces_and_loads_as_it_arrives() {
    let mut buf = LentBuf::new();
    let (mut a, _) = mock(MockConfig { lent: Some(buf.lent()), gpu_unit: true, ..Default::default() });
    for w in [3u32, 600, 1100, 2000] {
        a.write(reg::LENT_STORE + w * 8, 64, w as u64 + 1);
    }
    a.write(reg::DATA, 32, 42);
    a.write(reg::IRQ_SET, 32, 4);
    a.quiesce(u64::MAX).unwrap();
    let mut chunks = Vec::new();
    a.save(&mut chunks).unwrap();
    let shadows: Vec<u64> = chunks.iter().filter(|c| c.name == "shadow").map(|c| c.index).collect();
    assert_eq!(shadows, [0, 1, 2, 3], "zero pieces are left out and the rest come a piece at a time");
    assert_eq!(a.state_version(), STATE_VERSION);

    let mut buf2 = LentBuf::new();
    let (mut b, _) = mock(MockConfig { lent: Some(buf2.lent()), gpu_unit: true, ..Default::default() });
    b.quiesce(u64::MAX).unwrap();
    b.load(&mut VecSource::new(chunks.clone())).unwrap();
    assert_eq!((sum(&mut b), b.irq_levels()), (Read::Value(42), 4));
    b.observe_lent(0, 3000 * 8).unwrap();
    assert_eq!(buf2.lent().load(1100), 1101);

    // A state that fails part way has been applied up to there: the shell resets.
    let mut cut = chunks.clone();
    cut.last_mut().unwrap().bytes.truncate(5);
    let mut buf3 = LentBuf::new();
    let (mut c, _) = mock(MockConfig { lent: Some(buf3.lent()), gpu_unit: true, ..Default::default() });
    c.quiesce(u64::MAX).unwrap();
    assert!(matches!(c.load(&mut VecSource::new(cut)), Err(Refused::Mismatch(_))));
    assert_eq!(c.irq_levels(), 4, "the first chunks were applied");
    c.reset().unwrap();
    assert_eq!((c.irq_levels(), sum(&mut c)), (0, Read::Value(0)));
}

#[test]
fn a_faulted_engine_answers_as_the_contract_says_on_every_entry() {
    let mut buf = LentBuf::new();
    let (mut e, _) = mock(MockConfig { lent: Some(buf.lent()), gpu_unit: true, ..Default::default() });
    e.write(reg::IRQ_SET, 32, 2);
    e.fail("the mock stopped");
    e.fail("a second fault");
    assert_eq!(e.fault().as_deref(), Some("the mock stopped"), "the first fault stays");
    assert_eq!(e.write(reg::DATA, 32, 1), Write::Fault);
    assert_eq!(e.push_words(0, &[1]), 0);
    assert_eq!(e.read(reg::SUM, 32), Read::Fault);
    assert!(!e.access_waits(reg::SUM, 32, false) && !e.access_waits(reg::DATA, 32, true));
    assert_eq!(e.run(10), 0);
    assert_eq!(e.advance(1).idle, Idle::Faulted);
    assert_eq!(e.wait_idle(1, 1), Idle::Faulted);
    assert_eq!(e.observe(&(0, 0)), Err(Fault("the mock stopped".into())));
    assert_eq!(e.observe_lent(0, 8), Err(Fault("the mock stopped".into())));
    let faulted = Refused::Faulted("the mock stopped".into());
    assert_eq!(e.observe_all(), Err(faulted.clone()));
    assert_eq!(e.save(&mut Vec::new()), Err(faulted.clone()));
    assert_eq!(e.load(&mut VecSource::new(vec![])), Err(faulted.clone()));
    assert_eq!(e.quiesce(1), Err(faulted), "it reports the fault and still parks");
    // What keeps working, so the shell can see why.
    assert_eq!(e.irq_levels(), 2);
    assert!(drain(&mut e).contains(&Event::DeviceFault("the mock stopped".into())));
    e.resume();
    e.reset().unwrap();
    assert_eq!((e.fault(), e.write(reg::DATA, 32, 1)), (None, Write::Done));
}

#[test]
fn the_outbox_stays_bounded_and_drains_in_pieces() {
    let (mut e, _) = mock(MockConfig { queue_ops: 1 << 16, ..Default::default() });
    for i in 0..40_000u32 {
        e.set_source(Source { id: i % 3, charged: true });
        e.write(reg::DATA, 32, 1);
        e.write(reg::IRQ_SET, 32, 1);
    }
    e.run(usize::MAX);
    let mut first = Vec::new();
    e.drain_events(&mut first, 2);
    assert_eq!(first.len(), 2);
    let mut rest = Vec::new();
    e.drain_events(&mut rest, usize::MAX);
    let all: Vec<Event> = first.into_iter().chain(rest).collect();
    assert_eq!(all.len(), 4, "one Irq and three sources, however many stores: {all:?}");
    let retired: u32 = all.iter().filter_map(|x| if let Event::Credits { retired, .. } = x { Some(*retired) } else { None }).sum();
    assert_eq!(retired, 40_000);
}

#[test]
fn damage_joins_and_degrades_to_anything() {
    let o = Outbox::new(None);
    let m = |s: u64, x: i32| Mark { store: s, planes: 1, rect: Rect::new(x, x, x + 1, x + 1) };
    o.damage(&[m(1, 0), m(1, 5), m(2, 0)]);
    let mut v = Vec::new();
    o.drain(&mut v, usize::MAX);
    assert_eq!(v, vec![Event::Damage(Mark { store: 1, planes: 1, rect: Rect::new(0, 0, 6, 6) }), Event::Damage(m(2, 0))]);
    let many: Vec<Mark> = (0..40).map(|s| m(s, s as i32)).collect();
    o.damage(&many);
    o.drain(&mut v, usize::MAX);
    assert!(v.iter().any(|e| matches!(e, Event::Damage(d) if d.store == DAMAGE_ANY)), "{v:?}");
    assert!(v.len() < 20, "bounded: {}", v.len());
}

#[test]
fn an_interrupt_packet_replaces_the_queued_one_for_its_line() {
    let o = Outbox::new(None);
    let p = |vector: u8, level: bool| Event::Interrupt(Interrupt { sender: 7, widget: 8, addr: 0x80, vector, level });
    for e in [p(3, true), p(4, true), p(3, true), p(4, false), p(3, false), p(3, true)] {
        o.post(e);
    }
    o.post(Event::Interrupt(Interrupt { sender: 7, widget: 9, addr: 0x80, vector: 3, level: true }));
    let mut v = Vec::new();
    o.drain(&mut v, usize::MAX);
    let other = Event::Interrupt(Interrupt { sender: 7, widget: 9, addr: 0x80, vector: 3, level: true });
    assert_eq!(v, vec![p(4, false), p(3, true), other], "one per line, the newest state, in arrival order of those");
    for i in 0..100_000u32 {
        o.post(p(1, i % 2 == 0));
    }
    o.drain(&mut v, usize::MAX);
    assert_eq!(v.len(), 4, "a storm on one line stays one packet");
}

#[test]
fn two_senders_of_one_address_and_vector_are_two_lines() {
    let o = Outbox::new(None);
    let from = |sender: u8| Event::Interrupt(Interrupt { sender, widget: 8, addr: 0x80, vector: 3, level: true });
    for _ in 0..3 {
        o.post(from(1));
        o.post(from(2));
    }
    let mut v = Vec::new();
    o.drain(&mut v, usize::MAX);
    assert_eq!(v, vec![from(1), from(2)]);
    o.post(from(1));
    o.drain(&mut v, usize::MAX);
    assert_eq!(v.len(), 3, "once the shell has drained a packet, the next equal one is queued again");
}

#[test]
fn notify_fires_once_until_drained_or_rearmed() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let n = std::sync::Arc::new(AtomicU32::new(0));
    let n2 = n.clone();
    let o = Outbox::new(Some(Box::new(move || {
        n2.fetch_add(1, Ordering::SeqCst);
    })));
    o.post(Event::Irq);
    o.post(Event::Note("a".into()));
    assert_eq!(n.load(Ordering::SeqCst), 1);
    o.drain(&mut Vec::new(), usize::MAX);
    o.post(Event::Irq);
    assert_eq!(n.load(Ordering::SeqCst), 2, "a drain re-arms it");
    o.rearm();
    o.wake();
    assert_eq!(n.load(Ordering::SeqCst), 3);
}

#[test]
fn dirty_blocks_merge_neighbours_and_clip_at_the_end() {
    let len = 5 * DIRTY_BLOCK - 100;
    let d = DirtyBlocks::new(len);
    d.mark(0, 1);
    d.mark(DIRTY_BLOCK - 1, 2);
    d.mark(3 * DIRTY_BLOCK, 10);
    d.mark(len - 1, 1);
    d.mark(len + 50, 10);
    let mut v = Vec::new();
    d.drain(&mut v);
    assert_eq!(v, vec![DirtyRun { offset: 0, len: 2 * DIRTY_BLOCK }, DirtyRun { offset: 3 * DIRTY_BLOCK, len: 2 * DIRTY_BLOCK - 100 }]);
}

#[test]
fn lent_refuses_a_bad_region() {
    let mut buf = vec![0u64; 4];
    let p = buf.as_mut_ptr().cast::<u8>();
    unsafe {
        assert_eq!(Lent::new(p, 32).unwrap().as_ptr().cast::<u8>(), p);
        assert!(Lent::new(p, 31).is_none(), "not whole words");
        assert!(Lent::new(p.add(1), 24).is_none(), "not aligned");
        assert!(Lent::new(std::ptr::null_mut(), 8).is_none());
    }
}

#[test]
fn a_stopped_stream_is_idle_stopped_and_not_a_fault() {
    let (mut e, _) = mock(MockConfig::default());
    e.write(reg::DATA, 32, 5);
    e.write(reg::STOP, 32, 0);
    e.write(reg::DATA, 32, 7);
    assert_eq!(drain_stalls(&mut e, &mut 0, 0, usize::MAX), Idle::Stopped, "the shell's loop ends on it");
    assert_eq!((e.fault(), e.advance(1).idle), (None, Idle::Stopped));
    assert_eq!(e.write(reg::DATA, 32, 9), Write::Done, "a stopped stream takes no work but answers");
    assert_eq!(sum(&mut e), Read::Value(5), "the work after the stop never ran");
    assert_eq!(quiesce(&mut e, &mut 0, 0, 4), Ok(()));
    assert_eq!(e.save(&mut Vec::new()), Ok(()));
    e.resume();
    e.write(reg::STREAM_RESET, 32, 0);
    e.write(reg::DATA, 32, 1);
    assert_eq!(drain_stalls(&mut e, &mut 0, 0, usize::MAX), Idle::Yes, "the guest's reset starts it again");
    assert_eq!(sum(&mut e), Read::Value(6));
}

#[test]
fn a_write_at_gives_the_engine_the_shells_time_and_defaults_to_a_write() {
    let (mut e, _) = mock(MockConfig::default());
    assert_eq!(e.write_at(123, reg::DATA, 32, 4), Write::Done);
    assert_eq!(e.write_at(456, reg::DATA, 32, 5), Write::Done);
    e.run(usize::MAX);
    assert_eq!(sum(&mut e), Read::Value(9));
}

#[test]
fn heads_scan_out_and_each_has_its_own_deadline() {
    let (mut e, _) = mock(MockConfig::default());
    assert_eq!(e.heads(), 2);
    let mut px = vec![99; 3];
    assert_eq!(e.scanout(1, &mut px), Ok(Some((4, 2))));
    assert_eq!((px.len(), px[0] >> 16), (8, 2));
    assert_eq!(e.scanout(2, &mut px), Ok(None), "no such head");
    assert!(px.is_empty());
    assert_eq!((e.head_deadline(0), e.head_deadline(1), e.head_deadline(2)), (Some(1_000), Some(2_000), None));
    assert!(e.lend_frame(0).unwrap().is_none(), "no GPU scan-out");
    e.fail("gone");
    assert_eq!(e.scanout(0, &mut px), Err(Fault("gone".into())));
}

#[test]
fn a_frame_composes_into_the_callers_buffer_and_the_generation_says_when_it_changed() {
    let (mut e, _) = mock(MockConfig::default());
    assert_eq!((e.frame_size(0), e.frame_size(1), e.frame_size(2)), (Some((4, 2)), Some((4, 2)), None), "sized without composing");
    let g0 = e.frame_generation(0);
    assert_eq!(e.frame_generation(0), g0, "nothing happened");
    assert_eq!(e.frame_generation(2), 0, "no such head keeps none");
    let buf = parking_lot::Mutex::new(vec![0u32; 16]);
    assert_eq!(e.compose_into(1, &buf, 8), Ok(Some((4, 2))));
    {
        let words = buf.lock();
        assert_eq!((words[8 + 1] >> 16, words[3] & 0xffff, words[4..8].to_vec()), (2, 3, vec![0; 4]), "rows of the caller's stride, the padding untouched");
    }
    assert_eq!(e.compose_into(2, &buf, 8), Ok(None));
    assert!(e.compose_into(0, &buf, 2).is_err(), "a stride under the width");
    assert!(e.compose_into(0, &parking_lot::Mutex::new(vec![0u32; 3]), 4).is_err(), "a buffer too small");
    assert_eq!(e.write(reg::DATA, 32, 1), Write::Done);
    assert_ne!(e.frame_generation(0), g0, "a store changes it");
    let g1 = e.frame_generation(0);
    e.quiesce(u64::MAX).unwrap();
    e.reset().unwrap();
    assert_ne!(e.frame_generation(0), g1, "and so does a reset");
    let lent = LentFrame { width: 4, height: 2, stride: 4, words: FrameWords::Abgr, generation: 7, buffer: Arc::new(0u8) };
    assert_eq!((lent.words, lent.generation), (FrameWords::Abgr, 7));
}

#[test]
fn a_head_reports_its_timing_or_none_and_widens_its_colours_to_ten_bits() {
    let (mut e, _) = mock(MockConfig::default());
    assert_eq!(e.head_timing(0), None, "none loaded: the shell keeps its default");
    let t = e.head_timing(1).unwrap();
    assert_eq!((t.frame_clocks, t.clock_hz, t.fields, t.lines, t.epoch_ns, t.generation), (1_000_000, 60_000_000, 2, 525, -1, 3));
    assert_eq!(e.head_timing(2), None);
    // The mock's 8-bit frame, widened: the top 8 bits of each ten are the 8-bit colour.
    let mut eight = Vec::new();
    let mut ten = Vec::new();
    assert_eq!(e.scanout(1, &mut eight), Ok(Some((4, 2))));
    assert_eq!(e.scanout10(1, &mut ten), Ok(Some((4, 2))));
    for (a, b) in eight.iter().zip(&ten) {
        let (r, g, bl) = (a >> 16 & 0xff, a >> 8 & 0xff, a & 0xff);
        assert_eq!((b >> 22, b >> 12 & 0xff, b >> 2 & 0xff), (r, g, bl), "{a:#x} -> {b:#x}");
        assert_eq!(b >> 30, 0, "the two unused bits");
    }
    assert_eq!(e.scanout10(2, &mut ten), Ok(None));
    assert_eq!(ten.len(), 0);
}
