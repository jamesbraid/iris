//! The GR2 engine through `BoardEngine`: the picture, the probe, the read port, a stall that only the
//! shell's time ends, and saved state.

mod support;

use sgi_engine::shell::{drain_stalls, quiesce};
use sgi_engine::{BoardEngine, Chunk, Event, Idle, Read, Refused, StallOn, VecSource, Write};
use sgi_gr2::dev::gr2::{hq2, re3, AuxWindow, Gr2Probe, Gr2Variant};
use support::*;

/// The text-port scene iris pins its composed frame to (`gr2_composed_frame_is_pinned`), through the
/// engine's register window.
fn scene(rig: &mut Rig) {
    rig.tp_init();
    rig.program_display();
    rig.cmd(hq2::HQ2_2D_BEGIN, 0);
    rig.cmd(hq2::HQ2_2D_MODE, 0x1009);
    rig.cmd(hq2::HQ2_2D_COLOR_AUX, 0);
    rig.cmd(hq2::HQ2_2D_ROP, 0);
    for v in [0xff, 3, 0] {
        rig.data(v);
    }
    rig.cmd(hq2::HQ2_2D_TILE_SETUP, 0x40);
    for v in [85, 67, 2, 0xd022] {
        rig.data(v);
    }
    let mut words = Vec::new();
    for y in 0..67u32 {
        for w in 0..22u32 {
            let mut v = 0;
            for k in 0..4 {
                let x = w * 4 + k;
                let p = if x == 84 { 0xee } else if x < 84 { y + 1 } else { 0 };
                v |= p << (24 - 8 * k);
            }
            words.push(v);
        }
    }
    words.resize(1480, 0);
    rig.cmd(hq2::HQ2_2D_TILE_DATA_FIRST, words[0]);
    for &v in &words[1..] {
        rig.data(v);
    }
    rig.cmd(hq2::HQ2_2D_TILE_RECT_ODD, 0);
    for v in [40, 210, 16, 16, 295, 83] {
        rig.data(v);
    }
    rig.idle();
}

/// iris's frame pin: blake3 of the size and the `0xAABBGGRR` words, from `0x00RRGGBB` ones.
fn iris_hash(w: u32, h: u32, px: &[u32]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&w.to_le_bytes());
    hasher.update(&h.to_le_bytes());
    for p in px {
        let abgr = 0xff00_0000 | ((p & 0xff) << 16) | (p & 0xff00) | ((p >> 16) & 0xff);
        hasher.update(&abgr.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// iris's own pin for this scene, from `src/dev/gr2/hq2_tests.rs`.
const IRIS_FRAME_PIN: &str = "8522d00003541205becd1900df477acb6fdd992aab70ac125f451e5c5dca0999";

fn picture(rig: &mut Rig) -> (u32, u32, Vec<u32>) {
    let mut px = Vec::new();
    let (w, h) = rig.e.scanout(0, &mut px).unwrap().expect("a head");
    (w, h, px)
}

#[test]
fn the_picture_is_the_one_iris_composes() {
    for variant in [Gr2Variant::Xz, Gr2Variant::Extreme] {
        let mut rig = Rig::new(variant);
        scene(&mut rig);
        let (w, h, px) = picture(&mut rig);
        assert_eq!((w, h, px.len()), (1280, 1024, 1280 * 1024));
        assert_eq!(iris_hash(w, h, &px), IRIS_FRAME_PIN, "{variant:?}");
        assert!(px.iter().all(|p| p >> 24 == 0), "0x00RRGGBB");
    }
}

#[test]
fn a_head_the_board_lacks_has_no_picture() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    assert_eq!(rig.e.heads(), 1);
    let mut px = Vec::new();
    assert_eq!(rig.e.scanout(1, &mut px).unwrap(), None);
    assert_eq!(rig.e.frame_size(1), None);
    assert_eq!(rig.e.frame_size(0), Some((1280, 1024)));
}

#[test]
fn damage_and_the_frame_generation_follow_drawing() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.tp_init();
    rig.idle();
    let mut events = Vec::new();
    rig.e.advance(1);
    rig.e.drain_events(&mut events, usize::MAX);
    assert!(events.iter().any(|e| matches!(e, Event::Damage(_))), "the clear shows: {events:?}");
    let g = rig.e.frame_generation(0);
    rig.e.advance(2);
    assert_eq!(rig.e.frame_generation(0), g, "nothing drew");
    rig.rect(5, 10, 20, 12, 21);
    rig.idle();
    assert_ne!(rig.e.frame_generation(0), g, "the board has changed before the pump took it");
    rig.e.frame(3);
    events.clear();
    rig.e.drain_events(&mut events, usize::MAX);
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Damage(_))).count(), 1, "{events:?}");
}

#[test]
fn the_probe_answers_what_the_shell_configured() {
    // An Indigo-style board: one geometry engine, other port bytes, another HQ2 revision.
    let probe = Gr2Probe { ges: 1, ports: [0xdd, 0x3e, 0xf3, 0x15], hq_rev: 0, aux_window: AuxWindow::None };
    let mut rig = Rig::with_probe(probe);
    for (i, want) in probe.ports.iter().enumerate() {
        assert_eq!(rig.e.read(0x6c000 + 4 * i as u32, 8), Read::Value(*want as u64), "port {i} as a byte");
        assert_eq!(rig.e.read(0x6c000 + 4 * i as u32, 32), Read::Value(*want as u64), "port {i} as a word");
    }
    assert_eq!(rig.r32(0x6a040) >> 23, 0, "HQ2 revision");
    // Geometry engine slots past the count do not answer.
    rig.w32(0x68000 + 0x400 + 4, 0x5555);
    assert_eq!(rig.r32(0x68000 + 0x400 + 4), 0);
    rig.w32(0x68000 + 4, 0x5555);
    assert_eq!(rig.r32(0x68000 + 4), 0x5555);
    // No byte window on this board: a store there is not kept.
    rig.w32(0x6c1e0, 0x55);
    assert_eq!(rig.r32(0x6c1e0), 0);
    // iris's own boards.
    let mut xz = Rig::new(Gr2Variant::Xz);
    assert_eq!(xz.r32(0x6c000) & 0xf, !4 & 0xf);
    assert_ne!(xz.r32(0x6a040) >> 23, 0);
    xz.w32(0x6c1c0, 0x55);
    assert_eq!(xz.r32(0x6c1c0), 0x55, "iris's own word at +0x6c1c0");
    xz.w32(0x6c1e0, 0x55);
    assert_eq!(xz.r32(0x6c1e0), 0, "and its +0x6c1e0 never echoes");
    assert_eq!(xz.r32(0x6a07c), 0xdead_beef, "the probe word");
}

#[test]
fn a_read_that_waits_on_queued_work_answers_busy_until_the_board_runs() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.e.quiesce(u64::MAX).unwrap();
    // Parked, a write to the RE3 window queues and nothing runs it.
    let reg = 0x6c200 + 4 * re3::REG_FUNC as u32;
    assert_eq!(rig.e.write(reg, 32, 3), Write::Done);
    assert!(rig.e.access_waits(reg, 32, false));
    assert_eq!(rig.e.read(reg, 32), Read::Busy);
    assert_eq!(rig.e.wait_idle(0, 0), Idle::Working, "parked, it does not wait for work nobody runs");
    rig.e.resume();
    rig.idle();
    assert!(!rig.e.access_waits(reg, 32, false));
    assert!(matches!(rig.e.read(reg, 32), Read::Value(_)));
}

#[test]
fn only_a_command_word_takes_the_fast_store_path() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    assert_eq!(rig.e.try_store(0x40000 + 4 * hq2::PUC_COLOR, 32, 5), Write::Done);
    assert_eq!(rig.e.try_store(0x6c040, 32, 5), Write::Busy, "a register store has a side effect: write it");
    assert_eq!(rig.e.try_store(0x40000, 8, 5), Write::Busy, "and so does a narrow one");
    rig.idle();
}

/// The kernel's pixel read: a request through the FIFO, then the words come back on HQ2_GEDMA.
#[test]
fn the_read_port_hands_its_words_over_in_batches() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.cmd(hq2::HQ2_2D_BEGIN, 0);
    rig.cmd(hq2::HQ2_2D_MODE, 0x1009);
    rig.cmd(hq2::HQ2_2D_ROP, 0);
    for v in [0xff, 3, 0] {
        rig.data(v);
    }
    rig.cmd(hq2::HQ2_2D_DRAW_IMAGE, 100);
    for v in [50, 6, 2, 2, 2, 0] {
        rig.data(v);
    }
    for v in [0x0102_0304u32, 0x0506_0000, 0x1112_1314, 0x1516_0000, 0xdead_beef, 0xdead_beef] {
        rig.data(v);
    }
    rig.cmd(hq2::HQ2_2D_END_PRIMITIVE, 0);
    rig.cmd(hq2::HQ2_2D_BUF_SELECT, 2);
    rig.data(0);
    rig.w32(0x6a04c, 0);
    rig.cmd(hq2::HQ2_2D_DMA_READ_PIXELS, 100);
    for v in [50, 6, 2, 2, 0, 0] {
        rig.cmd(0, v);
    }
    let mut got = Vec::new();
    let mut buf = [0u32; 3];
    for _ in 0..1000 {
        let n = rig.e.pull_words(0x6a068, &mut buf);
        got.extend_from_slice(&buf[..n]);
        if got.len() >= 4 {
            break;
        }
        // Short: the port is empty for now; drive the board and ask again.
        drain_stalls(&mut rig.e, &mut rig.now, 1_000_000, 1);
    }
    assert_eq!(got, [0x0102_0304, 0x0506_0000, 0x1112_1314, 0x1516_0000]);
    assert_eq!(rig.r32(0x6a040) & 2, 2, "FIN2 after the transfer");
    rig.idle();
    assert!(!rig.e.access_waits(0x6a068, 32, false), "empty, and nothing is coming");
    assert_eq!(rig.e.pull_words(0x6a068, &mut buf), 0);
    assert_eq!(rig.e.pull_words(0x6a000, &mut buf), 0, "only the HQ2_GEDMA port is a read port");
}

#[test]
fn words_pushed_on_the_data_port_join_the_command_stream() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.cmd(hq2::HQ2_2D_BEGIN, 0);
    rig.cmd(hq2::HQ2_2D_MODE, 0x1009);
    rig.cmd(hq2::HQ2_2D_ROP, 0);
    for v in [0xff, 3, 0] {
        rig.data(v);
    }
    let (rows, wpr) = (400u32, 320u32);
    rig.w32(0x6a04c, 0);
    rig.cmd(hq2::HQ2_DMA_WRITE_PIXELS, 0);
    for v in [100, wpr * 4, rows, wpr, 0, 0] {
        rig.w32(0x40000, v);
    }
    let words: Vec<u32> = (0..rows * wpr).map(|i| i.wrapping_mul(0x0101_0101)).collect();
    let mut at = 0;
    while at < words.len() {
        let n = rig.e.push_words(0x6a068, &words[at..]);
        at += n;
        if n == 0 {
            drain_stalls(&mut rig.e, &mut rig.now, 1_000_000, 1);
        }
    }
    assert_eq!(rig.r32(0x6a040) & 2, 2, "FIN2 after the last row");
    rig.idle();
    assert!(rig.e.board().vram().iter().any(|&p| p != 0), "the rows were drawn");
    assert_eq!(rig.e.push_words(0x6c000, &[1]), 0);
    let mut events = Vec::new();
    rig.e.drain_events(&mut events, usize::MAX);
    assert!(events.iter().any(|e| matches!(e, Event::Note(n) if n.contains("not the HQ2_GEDMA data port"))), "{events:?}");
}

/// A shell whose bus cannot retry loops on `Busy`; the loop needs the shell's time to reach the end of
/// the stall, which the engine names.
#[test]
fn a_held_fin2_poll_is_released_by_the_shells_time() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.e.quiesce(u64::MAX).unwrap();
    rig.cmd(hq2::PUC_COLOR, 1);
    assert_eq!(rig.e.write(0x6a04c, 32, 0), Write::Done, "FIN2 awaited");
    rig.e.advance(5);
    assert_eq!(rig.e.read(0x6a040, 32), Read::Busy, "the HQ2 has work and FIN2 is clear");
    // Progress is judged over host time: the first look starts the clock, and a board that has moved
    // nothing for a quarter of a second is the stalled one.
    assert_eq!(rig.e.wait_idle(5, 0), Idle::Working);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let t0 = std::time::Instant::now();
    let Idle::Stalled(StallOn::Time(t)) = rig.e.wait_idle(5, 10_000_000_000) else { panic!("a stall only time ends") };
    assert!(t0.elapsed() < std::time::Duration::from_secs(1), "a stall returns at once, not after the budget");
    assert_eq!(t, 5 + 2_000_000_000);
    rig.e.advance(t + 1);
    assert!(matches!(rig.e.read(0x6a040, 32), Read::Value(v) if v & 2 == 0), "released, FIN2 still clear");
}

/// A FIN2 poll held while the HQ2 works through a long queue is progress, not a stall: the engine must
/// never tell the shell to jump its clock two seconds then, or the guest's own poll would end early.
#[test]
fn a_board_that_is_working_is_not_reported_stalled() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.cmd(hq2::HQ2_2D_BEGIN, 0);
    rig.w32(0x6a04c, 0);
    for _ in 0..20_000 {
        rig.cmd(hq2::PUC_COLOR, 1);
        rig.cmd(hq2::PUC_RECTI2D, 0);
        for v in [0, 63, 63] {
            rig.data(v);
        }
    }
    assert_eq!(rig.e.read(0x6a040, 32), Read::Busy, "the poll is held, so a stall deadline exists");
    assert!(rig.e.board().stall_deadline().is_some());
    let mut seen = 0;
    while !rig.e.board().idle() {
        let p = rig.e.advance(rig.now);
        assert!(!matches!(p.idle, Idle::Stalled(_)), "reported stalled after {seen} looks");
        assert!(!matches!(rig.e.wait_idle(rig.now, 1_000_000), Idle::Stalled(_)));
        seen += 1;
    }
    assert!(seen > 0, "the queue ran long enough to look at");
}

/// A bulk piece cut short would load as half a piece and zeros; it is refused.
#[test]
fn a_state_piece_of_the_wrong_length_is_refused() {
    let mut a = Rig::new(Gr2Variant::Xz);
    scene(&mut a);
    let mut now = 0;
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    a.e.save(&mut chunks).unwrap();
    for name in ["vram", "clut", "shram", "vc1sram"] {
        let mut torn = chunks.clone();
        let Some(i) = torn.iter().position(|c| c.name == name) else { continue };
        let half = torn[i].bytes.len() / 2;
        torn[i].bytes.truncate(half);
        let mut b = Rig::new(Gr2Variant::Xz);
        quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
        assert!(matches!(b.e.load(&mut VecSource::new(torn)), Err(Refused::Mismatch(_))), "{name} cut short");
    }
}

#[test]
fn a_saved_state_loads_into_another_engine_with_the_same_picture() {
    let mut a = Rig::new(Gr2Variant::Xz);
    scene(&mut a);
    let (_, _, shown) = picture(&mut a);
    let mut now = 0;
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    a.e.save(&mut chunks).unwrap();
    assert_eq!(chunks[0].name, "meta");
    let bytes: usize = chunks.iter().map(|c| c.bytes.len()).sum();
    assert!(bytes < 4 << 20, "the zero pieces of 15 MB of memory are left out: {bytes} bytes");

    let mut b = Rig::new(Gr2Variant::Xz);
    quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
    b.e.load(&mut VecSource::new(chunks.clone())).unwrap();
    let mut again = Vec::new();
    b.e.save(&mut again).unwrap();
    assert_eq!(again, chunks, "a loaded state saves as it was");
    b.e.resume();
    let (_, _, loaded) = picture(&mut b);
    assert!(shown == loaded, "the loaded picture differs from the one saved");
    // The loaded board draws on.
    b.rect(9, 600, 600, 640, 620);
    b.idle();
    let (_, _, drawn) = picture(&mut b);
    assert_ne!(drawn, loaded);
}

#[test]
fn a_state_of_another_board_is_refused_and_leaves_the_engine_as_it_was() {
    let mut a = Rig::new(Gr2Variant::Xz);
    let mut other = Rig::new(Gr2Variant::Extreme);
    let mut now = 0;
    quiesce(&mut other.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    other.e.save(&mut chunks).unwrap();
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    assert!(matches!(a.e.load(&mut VecSource::new(chunks)), Err(Refused::Mismatch(_))), "another engine count and revision");
    a.e.resume();
    a.tp_init();
    a.idle();
}

#[test]
fn reset_is_power_on() {
    let mut a = Rig::new(Gr2Variant::Xz);
    scene(&mut a);
    let mut now = 0;
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    a.e.reset().unwrap();
    a.e.resume();
    let mut fresh = Rig::new(Gr2Variant::Xz);
    let (_, _, p) = picture(&mut a);
    let (_, _, q) = picture(&mut fresh);
    assert!(p == q, "the picture is a fresh board's");
    let mut events = Vec::new();
    a.e.drain_events(&mut events, usize::MAX);
    assert_eq!(events.iter().filter(|e| **e == Event::Irq).count(), 1, "{events:?}");
}

#[test]
fn the_cursor_position_is_the_vc1s() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    assert!(!rig.e.cursor_position().2);
    rig.w32(0x6c040 + 0x18, 0x30);
    assert!(rig.e.cursor_position().2, "cursor and cursor display bits");
}

/// A shell may configure the byte cells; they hold what was stored and travel in the saved state.
#[test]
fn the_byte_cells_hold_and_are_saved_when_configured() {
    let probe = Gr2Probe { aux_window: AuxWindow::Bytes, ..Gr2Variant::Xz.probe() };
    let mut a = Rig::with_probe(probe);
    for (i, v) in [0u8, 0x55, 0xaa].into_iter().enumerate() {
        a.w8(0x6c1e0 + 4 * i as u32, v);
        assert_eq!(a.e.read(0x6c1e0 + 4 * i as u32, 8), Read::Value(v as u64));
    }
    let mut now = 0;
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    a.e.save(&mut chunks).unwrap();
    let mut b = Rig::with_probe(probe);
    quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
    b.e.load(&mut VecSource::new(chunks.clone())).unwrap();
    assert_eq!(b.e.read(0x6c1e8, 8), Read::Value(0xaa));
    // iris's own board saves its AB1 word too.
    let mut x = Rig::new(Gr2Variant::Xz);
    x.w32(0x6c1c0, 0xcafe_f00d);
    quiesce(&mut x.e, &mut now, 1_000_000, 100).unwrap();
    let mut xs: Vec<Chunk> = Vec::new();
    x.e.save(&mut xs).unwrap();
    let mut y = Rig::new(Gr2Variant::Xz);
    quiesce(&mut y.e, &mut now, 1_000_000, 100).unwrap();
    y.e.load(&mut VecSource::new(xs)).unwrap();
    assert_eq!(y.e.read(0x6c1c0, 32), Read::Value(0xcafe_f00d));
    let mut other = Rig::new(Gr2Variant::Xz);
    quiesce(&mut other.e, &mut now, 1_000_000, 100).unwrap();
    assert!(matches!(other.e.load(&mut VecSource::new(chunks)), Err(Refused::Mismatch(_))), "another window kind is another board");
}

/// Saved in the middle of a command, the interpreter's words so far go with the state: the loaded
/// engine takes the rest of the command and draws the rectangle (a fresh interpreter would drop the
/// words as strays).
#[test]
fn a_state_saved_mid_command_resumes_the_command() {
    let mut a = Rig::new(Gr2Variant::Xz);
    a.tp_init();
    a.idle();
    a.cmd(hq2::PUC_COLOR, 7);
    a.cmd(hq2::PUC_RECTI2D, 10);
    a.data(20);
    a.data(12);
    a.idle();
    let pixel = |r: &Rig, x: usize, y: usize| r.e.board().vram()[y * 1280 + x] & 0xff_ffff;
    assert_eq!(pixel(&a, 11, 21), 0, "one word short: nothing drawn yet");
    let mut now = 0;
    quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    a.e.save(&mut chunks).unwrap();
    assert!(chunks.iter().any(|c| c.name == "interp"));

    let mut b = Rig::new(Gr2Variant::Xz);
    quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
    b.e.load(&mut VecSource::new(chunks.clone())).unwrap();
    let mut again = Vec::new();
    b.e.save(&mut again).unwrap();
    assert_eq!(again, chunks, "a loaded state saves as it was, interpreter included");
    b.e.resume();
    b.data(21);
    b.idle();
    for (x, y) in [(10, 20), (12, 21), (11, 20)] {
        assert_eq!(pixel(&b, x, y), 7, "({x},{y})");
    }
    assert_eq!(pixel(&b, 13, 20), 0);
    // An interpreter chunk cut short is refused.
    let mut torn = chunks;
    let i = torn.iter().position(|c| c.name == "interp").unwrap();
    torn[i].bytes.truncate(100);
    quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
    assert!(matches!(b.e.load(&mut VecSource::new(torn)), Err(Refused::Mismatch(_))));
}
