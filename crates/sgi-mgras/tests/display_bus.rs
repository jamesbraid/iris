//! What the engine answers on the display bus where the guest's own software looks: the DAC's address register and pixel mask, and the video timing the VC3's tables and the DAC's PLL give.
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
