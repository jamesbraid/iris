//! What the engine answers on the display bus where the guest's own software looks: the DAC's address register, pixel mask and identity, the XMAP's and the VC3's revisions, and the video timing the VC3's tables and the DAC's PLL give.
//! Offsets are the Octane's `0x07xxxx` window less `0x10000`.

mod support;

use sgi_engine::{BoardEngine, Read, Write};
use sgi_mgras::dev::mgras::engine::{Board, Config, MgrasEngine, Mode};
use support::*;

fn put<E: BoardEngine>(e: &mut E, off: u32, bits: u32, val: u64) {
    assert_eq!(e.write(off, bits, val), Write::Done, "{off:#x}");
}

fn get<E: BoardEngine>(e: &mut E, off: u32, bits: u32) -> u64 {
    match e.read(off, bits) {
        Read::Value(v) => v,
        other => panic!("{off:#x}: {other:?}"),
    }
}

fn engine(config: impl FnOnce(&mut Config)) -> MgrasEngine<Mem> {
    let mut cfg = Config::new(Board::High, Mode::Inline);
    config(&mut cfg);
    MgrasEngine::new(cfg, Mem::default(), None, None).unwrap()
}

const DAC_ADDRESS: u32 = 0x61810;
const DAC_NUMBER: u32 = 0x61811;
const DAC_DATA: u32 = 0x61910;
const XMAP_INDEX: u32 = 0x61C88;
const XMAP_CONFIG: u32 = 0x61D00;
const VC3_INDEX: u32 = 0x62008;
const VC3_DATA: u32 = 0x62090;
const VC3_SRAM: u32 = 0x62190;

/// A DAC register through the byte ports: its number at `+1`, then its byte of data.
fn dac<E: BoardEngine>(e: &mut E, number: u32, value: u32) {
    put(e, DAC_NUMBER, 8, number as u64);
    put(e, DAC_DATA, 8, value as u64);
}

fn vc3<E: BoardEngine>(e: &mut E, index: u32, value: u32) {
    put(e, VC3_INDEX, 8, index as u64);
    put(e, VC3_DATA, 16, value as u64);
}

#[test]
fn the_dac_address_reads_back_as_the_halfword_written_and_a_byte_at_plus_one_sets_its_number() {
    let mut e = engine(|_| {});
    for pattern in [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0xFF] {
        put(&mut e, DAC_NUMBER, 8, pattern);
        assert_eq!(get(&mut e, DAC_ADDRESS, 16) >> 8 & 0xFF, pattern, "byte {pattern:#x}");
    }
    for pattern in [0x00, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0xFF] {
        put(&mut e, DAC_ADDRESS, 16, pattern << 8);
        assert_eq!(get(&mut e, DAC_ADDRESS, 16) >> 8 & 0xFF, pattern, "halfword {pattern:#x}");
    }
    // A number set either way names the same register: data written at it reads back at the other.
    put(&mut e, DAC_ADDRESS, 16, 0x0500);
    put(&mut e, DAC_DATA, 8, 0x89);
    put(&mut e, DAC_NUMBER, 8, 0x05);
    assert_eq!(get(&mut e, DAC_DATA, 8), 0x89);
}

#[test]
fn the_pixel_mask_written_through_the_byte_ports_blanks_the_frame_and_ff_shows_it_again() {
    let mut e = engine(|_| {});
    let mut out = Vec::new();
    // A colour map entry shows through the all-zero planes of window ID 0.
    put(&mut e, 0x60C08, 8, 0);
    put(&mut e, 0x60C88, 8, 0);
    put(&mut e, 0x60D00, 32, 0x112233);
    e.scanout(0, &mut out).unwrap();
    assert!(out.iter().any(|&p| p != 0), "something to blank");
    dac(&mut e, 4, 0);
    e.scanout(0, &mut out).unwrap();
    assert!(out.iter().all(|&p| p == 0), "blank");
    dac(&mut e, 4, 0xFF);
    e.scanout(0, &mut out).unwrap();
    assert!(out.iter().any(|&p| p != 0), "shown again");
}

#[test]
fn the_xmap_reports_the_configured_pp1_revision_under_index_4_and_a_capacity_of_one_under_index_8() {
    for rev in [0, 4, 7] {
        let mut e = engine(|c| c.pp1_revision = Some(rev));
        put(&mut e, XMAP_INDEX, 8, 1);
        put(&mut e, XMAP_CONFIG, 32, 0xFF00_0000);
        put(&mut e, XMAP_INDEX, 8, 4);
        assert_eq!(get(&mut e, XMAP_CONFIG, 32) & 7, rev as u64, "revision");
        put(&mut e, XMAP_INDEX, 8, 8);
        assert_eq!(get(&mut e, XMAP_CONFIG, 32) & 0xF, 1, "capacity");
    }
}

/// Without a revision given, the board's own, as the captured boards print it: A on the GE11 boards (and where no GE
/// revision is known), E on a one-GE GE12 board, H on a two-GE one.
#[test]
fn the_pp1_revision_follows_the_board_when_none_is_given() {
    let rev = |board, ge| {
        let mut e = engine(|c| (c.board, c.ge_revision) = (board, ge));
        put(&mut e, XMAP_INDEX, 8, 4);
        get(&mut e, XMAP_CONFIG, 32) & 7
    };
    assert_eq!([rev(Board::Solid, 0), rev(Board::High, 2), rev(Board::Max, 2)], [0, 0, 0], "GE11 or none: A");
    assert_eq!([rev(Board::Solid, 3), rev(Board::High, 3)], [4, 4], "one GE12: E");
    assert_eq!(rev(Board::Max, 3), 7, "two GE12: H");
}

/// PP1SELECT (0x071c08) reads back what was written whatever the XMAP index is: it was read at a key that carried the
/// index, so it read 0 once the kernel had selected an index. 0x071f00 is the DIB register file (gr4 CMAP.h
/// `XMAP_REG_DIB`), which the kernel stores to and the scan-out reads its buffer pointers from, so it stores
/// and reads back as written.
#[test]
fn pp1select_reads_back_at_any_index_and_the_dib_registers_store() {
    let mut e = engine(|_| {});
    for index in [0, 4, 8, 0x1c] {
        put(&mut e, XMAP_INDEX, 8, index);
        put(&mut e, 0x61C08, 8, 0x06);
        assert_eq!((get(&mut e, 0x61C08, 8), get(&mut e, 0x61C08, 32)), (0x06, 0x0600_0000), "index {index}");
        put(&mut e, 0x61C08, 8, 0x01);
        assert_eq!(get(&mut e, 0x61C08, 8), 0x01);
    }
    put(&mut e, XMAP_INDEX, 8, 0);
    put(&mut e, 0x61F00, 32, 0x1234_5678);
    assert_eq!(get(&mut e, 0x61F00, 32), 0x1234_5678);
}

#[test]
fn vc3_register_0x1f_has_its_revision_in_bits_7_and_6_and_stores_the_rest() {
    let mut e = engine(|_| {});
    vc3(&mut e, 0x1F, 0xFFFF);
    put(&mut e, VC3_INDEX, 8, 0x1F);
    assert_eq!(get(&mut e, VC3_DATA, 16), 0xFF3F, "revision 0, the other bits as stored");
}

/// The IRIX 1280x1024 60 Hz timing: 1065 lines of 840 VC3 clocks in a frame table at 0x380, one line
/// sequence at 0x400 of seven one-halfword entries of 120 clocks, the last one ending the line; the PLL
/// at r 2, v 4, control 0x91 (107.25 MHz, so a 53.625 MHz VC3 clock).
fn load_stock_timing<E: BoardEngine>(e: &mut E) {
    let sram = |e: &mut E, at: u32, words: &[u32]| {
        vc3(e, 7, at);
        put(e, VC3_INDEX, 8, 7);
        for &w in words {
            put(e, VC3_SRAM, 16, w as u64);
        }
    };
    let entry = |end: bool| 120 << 8 | 0x80 | if end { 0x8000 } else { 0 };
    sram(e, 0x400, &[entry(false), entry(false), entry(false), entry(false), entry(false), entry(false), entry(true)]);
    sram(e, 0x380, &[0x400, 1065, 0, 0]);
    vc3(e, 0, 0x380);
    dac(e, 0x0C, 2);
    dac(e, 0x0F, 4);
    dac(e, 0x09, 0x91);
}

#[test]
fn the_head_timing_follows_the_vc3_tables_and_the_pll_when_the_display_is_enabled() {
    let mut e = engine(|_| {});
    assert_eq!(e.head_timing(0), None, "nothing loaded: the shell keeps its default");
    load_stock_timing(&mut e);
    assert_eq!(e.head_timing(0), None, "the tables are read when the display is enabled");
    vc3(&mut e, 0x1E, 4);
    let t = e.head_timing(0).expect("a frame");
    // 894,600 VC3 clocks at 53.625 MHz, 59.943 Hz.
    assert_eq!(t.frame_clocks * 53_625_000, 894_600 * t.clock_hz as u64);
    assert_eq!((t.fields, t.lines, t.epoch_ns), (1, 1065, -1));
    assert_eq!(e.head_timing(1), None, "one head");

    vc3(&mut e, 0x1E, 4);
    assert_eq!(e.head_timing(0).unwrap().generation, t.generation, "an unchanged period keeps the phase");
    dac(&mut e, 0x0F, 5);
    vc3(&mut e, 0x1E, 4);
    let faster = e.head_timing(0).unwrap();
    assert_ne!(faster.generation, t.generation);
    assert!(faster.frame_clocks * 53_625_000 != 894_600 * faster.clock_hz as u64, "a new PLL, a new period");

    // A frame table that ends at once gives no frame; neither does one the display is not enabled for.
    vc3(&mut e, 0, 0x3FE);
    vc3(&mut e, 0x1E, 4);
    assert_eq!(e.head_timing(0), None);
}

#[test]
fn the_vc3_display_control_register_is_there_to_ask() {
    let mut e = engine(|_| {});
    assert_eq!(e.vc3_control(), 0);
    vc3(&mut e, 0x1E, 5);
    assert_eq!(e.vc3_control(), 5, "retrace interrupt and display enabled");
    vc3(&mut e, 0x1E, 4);
    assert_eq!(e.vc3_control(), 4);
    e.reset().unwrap();
    assert_eq!(e.vc3_control(), 0);
}

#[test]
fn a_reset_forgets_the_timing_and_the_generation_does_not_go_back() {
    let mut e = engine(|_| {});
    load_stock_timing(&mut e);
    vc3(&mut e, 0x1E, 4);
    let first = e.head_timing(0).unwrap().generation;
    e.reset().unwrap();
    assert_eq!(e.head_timing(0), None);
    load_stock_timing(&mut e);
    vc3(&mut e, 0x1E, 4);
    assert!(e.head_timing(0).unwrap().generation > first);
}

#[test]
fn the_dac_answers_the_id_and_revision_the_stock_diagnostic_accepts() {
    let mut e = engine(|_| {});
    put(&mut e, DAC_ADDRESS, 16, 0x0300);
    assert_eq!(get(&mut e, DAC_DATA, 8), 0x79, "ID");
    put(&mut e, DAC_ADDRESS, 16, 0x0B00);
    assert_eq!(get(&mut e, DAC_DATA, 8), 1, "revision");
}

/// Word reads of the bus's registers put the data in the top bytes, as the page says for the registers it lists with
/// both widths (the VC3's data register, 0x0720b0: "a 32-bit read puts it in bits 31:16"; the board-version and monitor
/// bytes: "bits 31:24") and as the bus's data rides (the byte registers are byte-wide, their 32-bit reads the byte
/// in bits 31:24). The old model's sweep of the VC3 registers, 34 patterns, reads back whole at both widths.
#[test]
fn the_vc3_sweep_reads_back_whole_and_word_reads_put_the_data_in_the_top_bytes() {
    let mut e = engine(|_| {});
    let data = 0x620b0;
    vc3(&mut e, 0x1E, 0);
    for step in 0..34u32 {
        let pattern = match step {
            0..=15 => 1 << step,
            16..=31 => !(1u32 << (step - 16)) & 0xFFFF,
            32 => 0,
            _ => 0xFFFF,
        };
        for reg in 0..=0x13u32 {
            put(&mut e, 0x62038, 32, (reg << 24 | pattern << 8) as u64);
            put(&mut e, VC3_INDEX, 8, reg as u64);
            assert_eq!(get(&mut e, data, 16), pattern as u64, "register {reg:#x}, pattern {pattern:#x}");
            assert_eq!(get(&mut e, data, 32), (pattern as u64) << 16, "register {reg:#x}, as a word");
        }
    }
    // The DAC mode byte: stored by a byte store or a word store (its top byte), read as a byte or as a word.
    put(&mut e, 0x61988, 8, 0x1C);
    assert_eq!((get(&mut e, 0x61988, 8), get(&mut e, 0x61988, 32)), (0x1C, 0x1C00_0000));
    put(&mut e, 0x61988, 32, 0x5A00_0000);
    assert_eq!((get(&mut e, 0x61988, 8), get(&mut e, 0x61988, 32)), (0x5A, 0x5A00_0000));
}

/// The shell publishes the guest's pointer from the VC3's cursor registers: the position registers (2 and 3) less the
/// 31 they are offset by, and the control register's (0x1d) enable, as the frame draws it.
#[test]
fn the_cursor_position_is_the_vc3s_registers_less_their_offset() {
    let mut e = engine(|_| {});
    assert_eq!(e.cursor_position(), (-31, -31, false), "reset: registers 0, hidden");
    vc3(&mut e, 0x02, 131);
    vc3(&mut e, 0x03, 231);
    assert_eq!(e.cursor_position(), (100, 200, false), "positioned, not shown");
    vc3(&mut e, 0x1D, 1);
    assert!(!e.cursor_position().2, "the display bit is needed too");
    vc3(&mut e, 0x1D, 3);
    assert_eq!(e.cursor_position(), (100, 200, true));
    vc3(&mut e, 0x02, 10);
    assert_eq!(e.cursor_position(), (-21, 200, true), "left of the screen: signed");
    vc3(&mut e, 0x1D, 0);
    assert!(!e.cursor_position().2);
}

fn vc3_read<E: BoardEngine>(e: &mut E, index: u32) -> u32 {
    put(e, VC3_INDEX, 8, index as u64);
    get(e, VC3_DATA, 16) as u32
}

/// The stock diagnostic's line waits (gr4 video-sync.md section 4.2, software expectation): VC3 index 0x0b reads 0 at the
/// start of a frame, passes 200 and wraps once a frame, from the shell's time and the loaded timing, and a loop that
/// only reads it still gets there. The frame here is 1065 lines in 16.68 ms (about 15.7 us a line).
#[test]
fn the_line_counter_is_the_beam_of_the_loaded_timing_and_wraps_every_frame() {
    let mut e = engine(|_| {});
    // Before a timing is loaded the counter steps per read, as it always has, and holds what is written.
    let (a, b) = (vc3_read(&mut e, 0xB), vc3_read(&mut e, 0xB));
    assert_eq!(b, a + 1);
    load_stock_timing(&mut e);
    assert_eq!(e.advance(5_000_000).idle, sgi_engine::Idle::Yes);
    vc3(&mut e, 0x1E, 4);
    let t = e.head_timing(0).unwrap();
    let frame_ns = (t.frame_clocks as u128 * 1_000_000_000 / t.clock_hz as u128) as u64;
    assert_eq!(vc3_read(&mut e, 0xB), 0, "line 0 at the start of the frame, the display having been enabled at the shell's time");
    // The shell's time moves the beam: half a frame on is about line 532, and a whole frame on is line 0 again.
    e.advance(5_000_000 + frame_ns / 2);
    let half = vc3_read(&mut e, 0xB);
    assert!((530..=535).contains(&half), "half a frame: {half}");
    e.advance(5_000_000 + frame_ns);
    assert!(vc3_read(&mut e, 0xB) < 3, "a whole frame: back at 0");
    // Without a time update a polling loop still sweeps the frame: it passes 200 and wraps within a frame's worth of reads.
    e.advance(5_000_000 + 2 * frame_ns);
    let mut seen_200 = false;
    let mut wrapped = false;
    let mut last = vc3_read(&mut e, 0xB);
    for _ in 0..40_000 {
        let l = vc3_read(&mut e, 0xB);
        seen_200 |= l >= 200;
        wrapped |= seen_200 && l < last;
        last = l;
    }
    assert!(seen_200 && wrapped, "a read-only poll reaches 200 and wraps");
    // A write holds the count until the display is enabled again, as before.
    vc3(&mut e, 0xB, 77);
    assert_eq!((vc3_read(&mut e, 0xB), vc3_read(&mut e, 0xB)), (77, 77));
    vc3(&mut e, 0x1E, 4);
    assert_ne!(vc3_read(&mut e, 0xB), 77);
}

/// The cursor X written to index 2 reaches index 0x0e at the next frame boundary (`mg_vc3cursorposition` writes X and Y,
/// waits two frames, and reads 0x0e back); with no timing loaded it takes the written X at once.
#[test]
fn the_current_cursor_x_follows_the_written_x_at_a_frame_boundary() {
    let mut e = engine(|_| {});
    vc3(&mut e, 2, 0x155);
    assert_eq!(vc3_read(&mut e, 0xE), 0x155, "no timing: at once");
    load_stock_timing(&mut e);
    e.advance(1_000_000);
    vc3(&mut e, 0x1E, 4);
    let t = e.head_timing(0).unwrap();
    let frame_ns = (t.frame_clocks as u128 * 1_000_000_000 / t.clock_hz as u128) as u64;
    vc3(&mut e, 2, 0x2aa);
    assert_eq!(vc3_read(&mut e, 0xE), 0x155, "inside the frame it was written in, the old X stays");
    e.advance(1_000_000 + frame_ns + 1);
    assert_eq!(vc3_read(&mut e, 0xE), 0x2aa, "a frame boundary later it has latched");
}

/// Display control (index 0x1e) bits 6:0 store and read back whole, the genlock bits 5 and 6 and the start and stop
/// sequence's bits 4:1 among them (software expectation, gr4 video-sync.md section 1.3); the shell reads bit 0, the
/// retrace interrupt enable, from `vc3_control`.
#[test]
fn display_control_bits_6_to_0_store_and_read_back() {
    let mut e = engine(|_| {});
    for v in [0x1E, 0x20, 0x40, 0x61, 0x7F, 0x00] {
        vc3(&mut e, 0x1E, v);
        assert_eq!(vc3_read(&mut e, 0x1E), v, "{v:#x}");
        assert_eq!(e.vc3_control(), v);
    }
}
