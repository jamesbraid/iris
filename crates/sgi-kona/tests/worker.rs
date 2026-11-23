//! The command stream on its worker thread: the effects and credits of the caller's own run, the PROM's bound on the
//! credit level, polls that do not wait, the drains before quiesce, save, load and reset, and one lock order.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sgi_engine::{shell, BoardEngine, Chunk, Event, HostMem, Idle, Interrupt, MemFault, Read, Refused, Source, StallOn, Swap, VecSource, Write};
use sgi_kona::dev::kona::back::textport::COLOUR_WORD;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

/// Guest memory that counts which path a DMA took: the worker's must be the RAM-only one.
#[derive(Default)]
struct Ram {
    bytes: Mutex<Vec<u8>>,
    ram: AtomicU64,
    bus: AtomicU64,
}

#[derive(Clone, Default)]
struct Guest(Arc<Ram>);

impl Guest {
    fn word(&self, addr: usize) -> u32 {
        let m = self.0.bytes.lock();
        m.get(addr..addr + 4).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()))
    }
}

impl HostMem for Guest {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        self.0.ram.fetch_add(1, SeqCst);
        let m = self.0.bytes.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| *b = m.get(addr as usize + i).copied().unwrap_or(0));
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        self.0.ram.fetch_add(1, SeqCst);
        let mut m = self.0.bytes.lock();
        if m.len() < addr as usize + data.len() {
            m.resize(addr as usize + data.len(), 0);
        }
        m[addr as usize..addr as usize + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn bus_read(&self, addr: u64, buf: &mut [u8], swap: Swap) -> Result<(), MemFault> {
        self.0.bus.fetch_add(1, SeqCst);
        self.ram_read(addr, buf, swap)
    }

    fn bus_write(&self, addr: u64, data: &[u8], swap: Swap) -> Result<(), MemFault> {
        self.0.bus.fetch_add(1, SeqCst);
        self.ram_write(addr, data, swap)
    }
}

type Engine = KonaEngine<Guest>;

const A: Source = Source { id: 1, charged: true };
const B: Source = Source { id: 2, charged: true };

fn config() -> KonaConfig {
    KonaConfig::ir2(2).unwrap().with_pool(0, 32)
}

fn pipe(threaded: bool) -> (Engine, Guest) {
    let guest = Guest::default();
    (Engine::new(config(), guest.clone(), Options { threaded, ..Options::default() }, None).expect("engine"), guest)
}

fn store(e: &mut Engine, off: u32, val: u32, src: Source) {
    e.set_source(src);
    loop {
        match e.write(off, 32, val as u64) {
            Write::Done => return,
            Write::Busy => std::thread::yield_now(),
            Write::Fault => panic!("fault: {:?}", e.fault()),
        }
    }
}

fn events(e: &mut Engine) -> Vec<Event> {
    let mut v = Vec::new();
    e.drain_events(&mut v, usize::MAX);
    v
}

fn credit_totals(events: &[Event]) -> std::collections::BTreeMap<u32, u32> {
    let mut t = std::collections::BTreeMap::new();
    for e in events {
        if let Event::Credits { source, retired } = e {
            *t.entry(*source).or_default() += retired;
        }
    }
    t
}

fn packets(events: &[Event]) -> Vec<Interrupt> {
    events.iter().filter_map(|e| if let Event::Interrupt(p) = e { Some(*p) } else { None }).collect()
}

/// Wait for the stream to have run.
fn settle(e: &mut Engine) {
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Yes);
}

/// A kernel command: its token by a slot store (the first word), the rest at the port.
fn kernel(e: &mut Engine, token: u32, words: &[u32], src: Source) {
    store(e, COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0], src);
    for &w in &words[1..] {
        store(e, COMMAND_PAGE, w, src);
    }
}

const REASON_PAGE: u32 = 0x31_0000;
const DRAW: [u32; 7] = [1, 2, 9, 1, 6, 7, 8];
const SWAP: [u32; 3] = [0x3c0001, 13, 1];

type Stores = Vec<(u32, u32, Source)>;

fn plain(s: &mut Stores, words: &[u32]) {
    words.iter().enumerate().for_each(|(i, &w)| s.push((COMMAND_PAGE, w, if i % 3 == 1 { B } else { A })));
}

fn token(s: &mut Stores, token: u32, words: &[u32]) {
    s.push((COMMAND_PAGE + (token << COMMAND_SLOT_SHIFT) + COMMAND_DATA_SLOT, words[0], A));
    words[1..].iter().for_each(|&w| s.push((COMMAND_PAGE, w, A)));
}

/// A stream that draws, takes two Hubs' stores and ends in a completion reason and its interrupt: a colour, boxes, a
/// bitmap and a point, the interrupt registers, the reason page and a reason.
fn script() -> Stores {
    let mut s = Stores::new();
    plain(&mut s, &[1, 2, 9, 1, 5, 2, 3, 20, 21]);
    plain(&mut s, &[1, BITMAP_SELECTOR, 32, 2, 0, 0, 0, 0, 0xffff_0000, 0x0f0f_0f0f]);
    plain(&mut s, &DRAW);
    for k in 0..40u32 {
        plain(&mut s, &[1, 2, k & 7, 1, 5, k, k, k + 30, k + 40]);
    }
    s.push((INT_DEST_LOWER, 0x2000, A));
    s.push((INT_VECTORS, 0x20000 | 0x11, A));
    s.push((INT_ENABLE, CAUSE_LOWATER, A));
    token(&mut s, PIPE_SET_UP, &[0x1000_0000 | REASON_PAGE >> 12, 0, 0x4000]);
    token(&mut s, CONTEXT_CLEAN_UP_WITH_EVENT_CODE, &[0x103]);
    s
}

/// What a finished run leaves, as a shell sees it.
struct Left {
    frame: Vec<u32>,
    state: Vec<Chunk>,
    credits: std::collections::BTreeMap<u32, u32>,
    packets: Vec<Interrupt>,
    reason: u32,
}

fn left(e: &mut Engine, guest: &Guest) -> Left {
    settle(e);
    e.quiesce(u64::MAX).unwrap();
    let mut state = Vec::new();
    e.save(&mut state).unwrap();
    let seen = events(e);
    Left { frame: e.compose().2, state, credits: credit_totals(&seen), packets: packets(&seen), reason: guest.word(REASON_PAGE as usize) }
}

/// The stream run by the caller: what the worker must leave.
fn inline_left(stream: &Stores) -> Left {
    let (mut e, guest) = pipe(false);
    for &(off, val, src) in stream {
        store(&mut e, off, val, src);
        e.run(usize::MAX);
    }
    left(&mut e, &guest)
}

/// The same stream through the caller and through the worker leaves the same frame, state, credits, packets and
/// guest memory, whatever the worker's timing: a poll or a pump between stores must not reorder anything.
#[test]
fn the_worker_leaves_what_the_callers_own_run_leaves() {
    let stream = script();
    let want = inline_left(&stream);
    assert_eq!(want.reason, 0x103);
    assert!(!want.packets.is_empty());

    for round in 0..8u32 {
        let (mut e, guest) = pipe(true);
        let mut seen = Vec::new();
        for (i, &(off, val, src)) in stream.iter().enumerate() {
            store(&mut e, off, val, src);
            // Polls, pumps and drains at every rhythm: none may change what the stream does.
            match (i as u32 + round) % 5 {
                0 => drop(e.read(CREDIT, 32)),
                1 => drop(e.advance(i as u64)),
                2 if round % 2 == 0 => std::thread::yield_now(),
                3 => seen.extend(events(&mut e)),
                _ => {}
            }
        }
        let mut got = left(&mut e, &guest);
        seen.extend(events(&mut e));
        // Credits and packets as the shell takes them, over the drains that came between.
        let mut totals = credit_totals(&seen);
        got.credits.iter().for_each(|(h, n)| *totals.entry(*h).or_default() += n);
        got.packets = packets(&seen).into_iter().chain(got.packets).collect();
        let lines = |p: &[Interrupt]| p.iter().map(|i| (i.addr, i.vector, i.level)).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(got.frame, want.frame, "round {round}: the frame differs");
        assert_eq!(got.state, want.state, "round {round}: the saved state differs");
        assert_eq!(totals, want.credits, "round {round}: the credits differ");
        assert_eq!(got.reason, want.reason, "round {round}: the reason did not reach its page");
        assert_eq!(lines(&got.packets), lines(&want.packets), "round {round}: the interrupts differ");
        assert_eq!((guest.0.bus.load(SeqCst), guest.0.ram.load(SeqCst) > 0), (0, true), "the worker's DMA takes the RAM-only path");
    }
}

/// A register the stream owns answers `Busy` until the stores before it have run, then reads what they wrote.
#[test]
fn a_read_the_stream_changes_waits_for_the_stores_before_it() {
    let (mut e, _) = pipe(true);
    for w in [1, 2, 9, 1, 5, 2, 3, 20, 21] {
        store(&mut e, COMMAND_PAGE, w, A);
    }
    store(&mut e, INT_VECTORS, 0x5a, A);
    let end = Instant::now() + Duration::from_secs(10);
    let v = loop {
        match e.read(INT_VECTORS, 32) {
            Read::Value(v) => break v,
            Read::Busy => assert!(Instant::now() < end, "the read never saw the stream run"),
            Read::Fault => panic!("{:?}", e.fault()),
        }
        std::thread::yield_now();
    };
    assert_eq!(v, 0x5a);
    assert_eq!(e.queued(), 0, "a read that waits answers once everything before it has run");
}

/// While the worker draws, the registers the driver polls and the questions the shell asks answer without waiting for
/// the store in flight: the worker's long run is not in them.
#[test]
fn polls_and_questions_do_not_wait_for_the_stream() {
    const GC: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
    let (mut e, _) = pipe(true);
    kernel(&mut e, WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC, A);
    // Whole-screen boxes drawn on the worker's own thread: a second of work.
    for _ in 0..1000 {
        kernel(&mut e, FILL_BOXES_TWO_CORNERS, &[0, 1023 << 16 | 1279], A);
    }
    let t0 = Instant::now();
    for _ in 0..1000 {
        for off in [INT_CAUSE, ACTIVITY, RETRACE_TAG, WIDGET_ID] {
            assert!(matches!(e.read(off, 32), Read::Value(_)), "{off:#x} answers at once");
        }
        let _ = (e.heads(), e.frame_size(0), e.head_timing(0), e.head_bits(0), e.frame_generation(0), e.channel_window(0), e.dac_cursor(), e.busy_ns(), e.swaps_dropped(), e.irq_levels());
    }
    // A level read may run a batch of the stores, so it is one read, and it still answers while the worker draws.
    let level = Instant::now();
    assert!(matches!(e.read(CREDIT, 32), Read::Value(_)));
    assert!(level.elapsed() < Duration::from_secs(1), "a level read took {:?}", level.elapsed());
    assert!(e.queued() > 0, "the worker was still drawing: the polls did not wait for it");
    assert!(t0.elapsed() < Duration::from_secs(1), "the polls and questions took {:?}", t0.elapsed());
    e.quiesce(u64::MAX).unwrap();
}

/// The PROM waits for the credit level to read zero for up to 5,000 reads before each textport command. With the
/// host's cores busy, a command's words are still taken by the worker well inside that bound, and every credit comes back.
#[test]
fn the_credit_level_reads_zero_inside_the_proms_bound_with_the_host_busy() {
    let stop = Arc::new(AtomicBool::new(false));
    let spinners: Vec<_> = (0..std::thread::available_parallelism().map_or(4, |n| n.get()).min(32))
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 1u64;
                while !stop.load(SeqCst) {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    let (mut e, _) = pipe(true);
    let mut worst = 0;
    let mut stores = 0;
    for command in 0..200u32 {
        for w in [1, 5, command & 7, command, 3, 30 + command & 63] {
            store(&mut e, COMMAND_PAGE, w, A);
            stores += 1;
        }
        let mut polls = 0;
        loop {
            polls += 1;
            // Roughly the cost of a guest's uncached load through the machine.
            let t = Instant::now();
            while t.elapsed() < Duration::from_nanos(1000) {
                std::hint::spin_loop();
            }
            match e.read(CREDIT, 32) {
                Read::Value(0) => break,
                Read::Value(_) => {}
                other => panic!("{other:?}"),
            }
            assert!(polls <= 5000, "command {command}: the level was still up after {polls} reads");
        }
        worst = worst.max(polls);
    }
    stop.store(true, SeqCst);
    spinners.into_iter().for_each(|s| s.join().unwrap());
    settle(&mut e);
    let _ = e.advance(0);
    let owed = credit_totals(&events(&mut e));
    assert_eq!(owed.get(&1), Some(&stores), "every charged store's credit came back (worst level wait: {worst} reads)");
}

/// Quiesce, save, load and reset drain the worker first: stores in flight are run, nothing is left queued, and the
/// state saved is the state of the whole stream; a second engine loads it and goes on to the same frame.
#[test]
fn quiesce_save_load_and_reset_drain_the_worker_first() {
    let stream = script();
    let want = inline_left(&stream).frame;
    let (mut e, _) = pipe(true);
    for &(off, val, src) in &stream {
        store(&mut e, off, val, src);
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    assert!(matches!(e.save(&mut chunks), Err(Refused::NotQuiesced)), "save needs a quiesce");
    assert!(matches!(e.load(&mut VecSource::new(Vec::new())), Err(Refused::NotQuiesced)));
    assert!(matches!(e.reset(), Err(Refused::NotQuiesced)));
    e.quiesce(u64::MAX).unwrap();
    assert_eq!(e.queued(), 0, "the stores in flight ran");
    let frame = e.compose().2;
    assert_eq!(frame, want, "and their drawing landed");
    e.save(&mut chunks).unwrap();

    let (mut f, _) = pipe(true);
    f.quiesce(u64::MAX).unwrap();
    f.load(&mut VecSource::new(chunks.clone())).unwrap();
    assert_eq!(f.compose().2, frame, "the loaded engine shows the saved frame");
    f.resume();
    store(&mut f, COMMAND_PAGE, 1, A);
    settle(&mut f);
    f.quiesce(u64::MAX).unwrap();
    f.resume();

    e.resume();
    for &w in &[1, 2, 5, 1, 5, 100, 100, 110, 110] {
        store(&mut e, COMMAND_PAGE, w, A);
    }
    e.quiesce(u64::MAX).unwrap();
    assert_eq!(e.queued(), 0);
    e.reset().expect("a quiesced engine resets");
    assert_eq!(e.pixel(7, 8, COLOUR_WORD), 0, "reset cleared the picture");
    e.resume();
    store(&mut e, COMMAND_PAGE, 1, A);
    settle(&mut e);
}

/// The shell's quiesce loop, bounded by wall time, not by a count of tries a loaded host can spend before the worker runs.
fn quiesce_within(e: &mut Engine, limit: Duration) {
    let end = Instant::now() + limit;
    let mut now = 0;
    loop {
        match shell::quiesce(e, &mut now, 1_000_000, 1) {
            Err(Refused::Midway(m)) if Instant::now() < end => drop(m),
            done => return done.expect("the loop gives the frame and the engine parks"),
        }
    }
}

/// A swap holds the worker at the frame boundary: what is behind it does not run until `frame`, which the shell's
/// loop gives, and a quiesce says so rather than hang.
#[test]
fn a_swap_holds_the_worker_until_the_frame() {
    let (mut e, _) = pipe(true);
    kernel(&mut e, DISPLAY_MODE_BYTE, &SWAP, A);
    for &w in &DRAW {
        store(&mut e, COMMAND_PAGE, w, A);
    }
    assert_eq!(e.wait_idle(0, u64::MAX), Idle::Stalled(StallOn::Frame));
    assert!(e.swap_waiting());
    assert_eq!(e.queued(), 7, "nothing behind the swap ran");
    assert_eq!(e.pixel(7, 8, COLOUR_WORD), 0);
    assert!(matches!(e.quiesce(u64::MAX), Err(Refused::Midway(_))));
    quiesce_within(&mut e, Duration::from_secs(60));
    assert_eq!((e.queued(), e.pixel(7, 8, COLOUR_WORD)), (0, 9));
}

/// Kill the process if `progress`, which only the thread under test moves, stops: a wrong lock order hangs, and a hung test says nothing.
fn watchdog(progress: Arc<AtomicU64>, done: Arc<AtomicBool>, what: &'static str) {
    std::thread::spawn(move || {
        let (mut last, mut since) = (progress.load(SeqCst), Instant::now());
        while !done.load(SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
            let now = progress.load(SeqCst);
            if now != last {
                (last, since) = (now, Instant::now());
            } else if since.elapsed() > Duration::from_secs(20) {
                eprintln!("NO PROGRESS in 20 s: deadlock ({what})");
                std::process::abort();
            }
        }
    });
}

/// One lock order: the engine's pipe, then the credits, then the register file. The worker gives a many-Hub packet's
/// credits (the credits, then the register file for each) while the caller's stores to the low window word park and
/// release them and a second thread composes rows; the quiesce, save, load and reset cycle takes the pipe under them.
/// A register file taken before the credits on the caller's side is a real AB-BA the worker meets within a round.
#[test]
fn two_threads_take_the_locks_in_one_order() {
    let (mut e, _) = pipe(true);
    let progress = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    watchdog(progress.clone(), done.clone(), "locks");
    let src = e.scan_source().expect("a source");
    let rows = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut out = vec![0u32; 1280 * 1024];
            while !done.load(SeqCst) {
                src.rows(0, 0, 64, &mut out).unwrap();
            }
        })
    };
    let end = Instant::now() + Duration::from_secs(4);
    let mut n = 0u32;
    while Instant::now() < end {
        // A bitmap packet whose 60 payload words come from 60 Hubs: when its last word completes it, the worker
        // gives 60 credits (the credits held, the register file taken 60 times) while the caller's stores to the
        // low window word take the register file and the credits the other way round if the order is wrong.
        let packet = [1, BITMAP_SELECTOR, 32, 60, 0, 0, 0, 0].into_iter().chain((0..60).map(|k| k ^ n));
        for (k, w) in packet.enumerate() {
            store(&mut e, COMMAND_PAGE, w, Source { id: 1 + k as u32 % 60, charged: true });
        }
        for k in 0..400 {
            store(&mut e, LOW_WINDOW_WRITE, k & 1, B);
        }
        store(&mut e, LOW_WINDOW_WRITE, 0, B);
        events(&mut e);
        if n % 16 == 0 {
            let mut chunks: Vec<Chunk> = Vec::new();
            quiesce_within(&mut e, Duration::from_secs(60));
            e.save(&mut chunks).unwrap();
            if n % 32 == 0 {
                e.load(&mut VecSource::new(chunks)).unwrap();
            } else {
                e.reset().unwrap();
            }
            e.resume();
        }
        n += 1;
        progress.fetch_add(1, SeqCst);
    }
    done.store(true, SeqCst);
    rows.join().unwrap();
    e.quiesce(u64::MAX).unwrap();
    assert!(n > 20, "only {n} rounds");
}

/// A run that does not drain still shows the shell what it changed: a format load ahead of a second of drawing is in
/// `head_timing` and the generation has moved while stores are still queued.
#[test]
fn a_saturated_worker_publishes_the_heads_and_the_generation_as_it_goes() {
    const GC: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];
    let (mut e, _) = pipe(true);
    assert_eq!(e.head_timing(0), None);
    let mut format = [0u32; 383];
    (format[1], format[19], format[20], format[21], format[22], format[23], format[24]) = (1, 60 << 16, 1, 1066, 1688, 1024, 1280);
    let sum = format.iter().fold(VIDEO_LOAD_FORMAT, |s, w| s.wrapping_add(*w));
    kernel(&mut e, WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC, A);
    // The format load sits behind boxes the worker is still drawing, so no run ends before it.
    for k in 0..1500 {
        if k == 200 {
            for w in [0x1a6, VIDEO_LOAD_FORMAT].into_iter().chain(format).chain([sum]) {
                store(&mut e, COMMAND_PAGE, w, A);
            }
        }
        kernel(&mut e, FILL_BOXES_TWO_CORNERS, &[0, 1023 << 16 | 1279], A);
    }
    // The pump hands over the stores still staged: the worker has all of them in one queue.
    e.advance(0);
    let g = e.frame_generation(0);
    let end = Instant::now() + Duration::from_secs(20);
    while e.head_timing(0).is_none() || e.frame_generation(0) == g {
        assert!(e.queued() > 0, "the worker drained before it published");
        assert!(Instant::now() < end, "no view or generation in 20 s");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(e.queued() > 0, "published while the stream was still queued");
    e.quiesce(u64::MAX).unwrap();
}

/// A completion reason that waits behind an unread one is posted by the cause read itself when the worker is idle,
/// as an inline engine posts it, not left for the worker to find.
#[test]
fn a_cause_read_posts_the_waiting_reason_when_the_worker_is_idle() {
    let (mut e, guest) = pipe(true);
    let mut s = Stores::new();
    token(&mut s, PIPE_SET_UP, &[0x1000_0000 | REASON_PAGE >> 12, 0, 0x4000]);
    token(&mut s, CONTEXT_CLEAN_UP_WITH_EVENT_CODE, &[0x101]);
    token(&mut s, CONTEXT_CLEAN_UP_WITH_EVENT_CODE, &[0x102]);
    for (off, val, src) in s {
        store(&mut e, off, val, src);
    }
    settle(&mut e);
    assert_eq!(guest.word(REASON_PAGE as usize), 0x101, "the second waits for the first to be read");
    // The kernel's handler reads word 0 and writes it back ORed with 0xbad00000.
    guest.0.bytes.lock()[REASON_PAGE as usize..][..4].copy_from_slice(&0xbad0_0101u32.to_be_bytes());
    assert!(matches!(e.read(INT_CAUSE, 32), Read::Value(_)));
    assert_eq!(guest.word(REASON_PAGE as usize), 0x102, "the read posted it");
    e.quiesce(u64::MAX).unwrap();
}

/// With the worker parked (a quiesce stops it) only reads can run stores: each command's level, and a full
/// queue's, reaches zero inside the PROM's 5,000 reads.
#[test]
fn the_credit_level_reaches_zero_inside_the_proms_bound_with_the_worker_parked() {
    let (mut e, _) = pipe(true);
    e.quiesce(u64::MAX).unwrap();
    for command in 0..50u32 {
        for w in [1, 5, command & 7, command, 3, 30 + command & 63] {
            store(&mut e, COMMAND_PAGE, w, A);
        }
        let polls = (1..=5000).find(|_| e.read(CREDIT, 32) == Read::Value(0));
        assert!(polls.is_some(), "command {command}: the level was still up after 5000 reads with the worker parked");
    }
    // A full queue drains in a bounded number of reads.
    for k in 0..60_000u32 {
        store(&mut e, COMMAND_PAGE, 1, if k % 2 == 0 { A } else { B });
    }
    let polls = (1..=5000).find(|_| e.read(CREDIT, 32) == Read::Value(0));
    assert!(polls.is_some(), "60,000 queued stores were still queued after 5000 reads");
    assert_eq!(e.queued(), 0);
}
