//! What the engine answers on the display bus where the guest's own software looks: the DAC's address register and pixel mask.
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

/// A DAC register through the byte ports: its number at `+1`, then its byte of data.
fn dac<E: BoardEngine>(e: &mut E, number: u32, value: u32) {
    put(e, DAC_NUMBER, 8, number as u64);
    put(e, DAC_DATA, 8, value as u64);
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
