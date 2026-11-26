// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-kona/tests/menu.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! The PROM maintenance menu replay and the three textport cases QEMU's shell qtests ran, against the engine
//! through its Rust interface. The replay is the old QEMU model's recorded register stream and frame.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use sgi_engine::{shell, BoardEngine, HostMem, Idle, MemFault, Read, Source, Swap, Write};
use sgi_kona::dev::kona::defs::COMMAND_PAGE;
use sgi_kona::dev::kona::engine::{KonaEngine, Options};
use sgi_kona::dev::kona::ident::KonaConfig;

#[derive(Clone, Default)]
struct Ram(Arc<Mutex<HashMap<u64, Box<[u8; 4096]>>>>);

impl HostMem for Ram {
    fn ram_read(&self, addr: u64, buf: &mut [u8], _: Swap) -> Result<(), MemFault> {
        let pages = self.0.lock();
        buf.iter_mut().enumerate().for_each(|(i, b)| {
            let a = addr + i as u64;
            *b = pages.get(&(a >> 12)).map_or(0, |p| p[(a & 0xfff) as usize]);
        });
        Ok(())
    }

    fn ram_write(&self, addr: u64, data: &[u8], _: Swap) -> Result<(), MemFault> {
        let mut pages = self.0.lock();
        data.iter().enumerate().for_each(|(i, b)| {
            let a = addr + i as u64;
            pages.entry(a >> 12).or_insert_with(|| Box::new([0; 4096]))[(a & 0xfff) as usize] = *b;
        });
        Ok(())
    }
}

type Engine = KonaEngine<Ram>;

fn engine(ram: Ram) -> Engine {
    Engine::new(KonaConfig::ir2(1).unwrap().with_pool(0, 32), ram, Options::default(), None).expect("engine")
}

/// Run the stream, giving the frames it waits for.
fn settle(e: &mut Engine) {
    if shell::drain_stalls(e, &mut 0, 1_000_000, usize::MAX) == Idle::Faulted {
        panic!("the pipe faulted: {:?}", e.fault());
    }
}

/// One textport command: the sync word, the selector, then its arguments.
fn textport(e: &mut Engine, selector: u32, args: &[u32]) {
    for w in [1, selector].iter().chain(args) {
        e.set_source(Source { id: 0, charged: false });
        assert_eq!(e.write(COMMAND_PAGE, 32, *w as u64), Write::Done);
        e.run(usize::MAX);
    }
}

/// The displayed pixel at wire (x, y), which counts rows up from the bottom, as 0x00RRGGBB.
fn shown(e: &mut Engine, x: u32, y: u32) -> u32 {
    settle(e);
    let (w, h, words) = e.compose();
    let p = words[((h - 1 - y) * w + x) as usize];
    (p & 0xff) << 16 | p & 0xff00 | (p >> 16) & 0xff
}

#[test]
fn a_colour_map_entry_and_a_point_draw_one_pixel_each() {
    let mut e = engine(Ram::default());
    textport(&mut e, 0x3, &[1, 0xff << 2, 0, 0]);
    textport(&mut e, 0x2, &[1]);
    textport(&mut e, 0x6, &[100, 50]);
    assert_eq!(shown(&mut e, 100, 50), 0x00ff0000);
    textport(&mut e, 0x3, &[2, 0, 0xff << 2, 0]);
    textport(&mut e, 0x2, &[2]);
    textport(&mut e, 0x6, &[200, 150]);
    assert_eq!(shown(&mut e, 200, 150), 0x0000ff00);
    assert_eq!(shown(&mut e, 100, 50), 0x00ff0000);
}

#[test]
fn a_filled_rectangle_leaves_out_its_left_and_bottom_edges() {
    let mut e = engine(Ram::default());
    textport(&mut e, 0x3, &[1, 0x1f << 2, 0x2f << 2, 0x3f << 2]);
    textport(&mut e, 0x2, &[1]);
    textport(&mut e, 0x5, &[10, 20, 12, 22]);
    for (x, y) in [(11, 21), (12, 21), (11, 22), (12, 22)] {
        assert_eq!(shown(&mut e, x, y), 0x001f2f3f, "({x}, {y})");
    }
    for (x, y) in [(10, 21), (10, 20), (11, 20), (13, 21), (11, 23)] {
        assert_eq!(shown(&mut e, x, y), 0, "({x}, {y})");
    }
}

#[test]
fn a_bitmap_blit_draws_its_set_bits_and_advances_the_raster_position() {
    let mut e = engine(Ram::default());
    textport(&mut e, 0x3, &[1, 0xab << 2, 0xcd << 2, 0xef << 2]);
    textport(&mut e, 0x2, &[1]);
    textport(&mut e, 0x4, &[300, 400]);
    textport(&mut e, 0xa, &[4, 2, 0, 0, 5, 0]);
    e.set_source(Source { id: 0, charged: false });
    assert_eq!(e.write(COMMAND_PAGE, 32, 0xa500_0000), Write::Done);
    e.run(usize::MAX);
    for (x, y, on) in [(300, 400, 1), (301, 400, 0), (302, 400, 1), (303, 400, 0), (300, 401, 0), (301, 401, 1), (302, 401, 0), (303, 401, 1)] {
        assert_eq!(shown(&mut e, x, y), if on == 1 { 0x00abcdef } else { 0 }, "({x}, {y})");
    }
    // The advance of 5 moved the position to (305, 400): a second blit chains from it.
    textport(&mut e, 0xa, &[4, 2, 0, 0, 0, 0]);
    assert_eq!(e.write(COMMAND_PAGE, 32, 0xa500_0000), Write::Done);
    e.run(usize::MAX);
    assert_eq!(shown(&mut e, 305, 400), 0x00abcdef);
    assert_eq!(shown(&mut e, 308, 401), 0x00abcdef);
}

fn num(s: &str) -> u64 {
    s.strip_prefix("0x").map_or_else(|| s.parse().unwrap(), |h| u64::from_str_radix(h, 16).unwrap())
}

/// The PROM's System Maintenance Menu: every register access of one boot to the pipe, as QEMU's old model
/// answered it. All reads match but the one-wire data bit (the old model's placeholder Ktown record), and
/// the frame is the model's recorded one, byte for byte.
#[test]
fn the_prom_maintenance_menu_replays_to_its_recorded_frame() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let text = std::fs::read_to_string(dir.join("maintenance-menu.replay")).expect("the menu replay");
    let ram = Ram::default();
    let mut e = engine(ram);
    let (mut reads, mut wrong) = (0, Vec::new());
    for (n, line) in text.lines().enumerate() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.first().copied() {
            Some("write") => {
                let (size, off, val) = (num(w[1]) as u32, num(w[2]) as u32, num(w[3]));
                e.set_source(Source { id: 1, charged: (0xc10000..0xc14000).contains(&off) });
                while e.write(off, size * 8, val) == Write::Busy {
                    settle(&mut e);
                }
                // The shell runs the stream after each store.
                e.run(usize::MAX);
            }
            Some("read") => {
                let (size, off, val) = (num(w[1]) as u32, num(w[2]) as u32, num(w[3]));
                let mask = w.get(4).map_or(u64::MAX, |m| num(m));
                let mut got = 0u64;
                for k in 0..size / 4 {
                    got = got << 32
                        | loop {
                            match e.read(off + 4 * k, 32) {
                                Read::Value(v) => break v,
                                Read::Busy => settle(&mut e),
                                Read::Fault => panic!("line {}: the pipe faulted: {:?}", n + 1, e.fault()),
                            }
                        };
                }
                reads += 1;
                if (got ^ val) & mask != 0 && wrong.len() < 8 {
                    wrong.push(format!("line {}: read {off:#x} gave {got:#x}, expected {val:#x} (mask {mask:#x})", n + 1));
                }
            }
            _ => {}
        }
    }
    settle(&mut e);
    assert_eq!(reads, 9106);
    assert!(wrong.is_empty(), "{wrong:#?}");
    let (w, h, words) = e.compose();
    let ppm = std::fs::read(dir.join("maintenance-menu.ppm")).expect("the recorded frame");
    let header = format!("P6\n{w} {h}\n255\n").into_bytes();
    assert_eq!(&ppm[..header.len()], &header[..]);
    let frame: Vec<u8> = words.iter().flat_map(|p| [*p as u8, (*p >> 8) as u8, (*p >> 16) as u8]).collect();
    assert!(ppm[header.len()..] == frame[..], "the menu frame differs from the recorded one");
}

/// Records of the menu replay taken as one frame by the damage test.
const FRAME_RECORDS: usize = 500;

/// Composing only the rows the engine marks, after every `FRAME_RECORDS` records of the menu, shows what a whole
/// compose does, and most frames mark a part of the picture.
#[test]
fn composing_the_marked_rows_of_the_menu_gives_the_frame_a_whole_compose_does() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let text = std::fs::read_to_string(dir.join("maintenance-menu.replay")).expect("the menu replay");
    let mut e = engine(Ram::default());
    let (mut shown, mut whole) = (Vec::new(), Vec::new());
    let (mut frames, mut full, mut named) = (0, 0, 0);
    // The head has no picture until the PROM loads its video format; the first look at it is everything.
    let mut compare = |e: &mut Engine, shown: &mut Vec<u32>, at: usize| {
        settle(e);
        let Some((w, h)) = BoardEngine::scanout(e, 0, &mut whole).unwrap() else { return };
        let (w, h) = (w as usize, h as usize);
        shown.resize(w * h, 0);
        if let Some((a, b)) = e.take_damage(0) {
            assert!(e.scan_source().expect("a source").rows(0, a as usize, b as usize, shown).is_ok());
            (frames, full, named) = (frames + 1, full + h, named + (b - a) as usize);
        }
        if let Some(y) = (0..h).find(|&y| shown[y * w..(y + 1) * w] != whole[y * w..(y + 1) * w]) {
            panic!("record {at}: row {y} is stale after composing the damage");
        }
    };
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.first().copied() {
            Some("write") => {
                let (size, off, val) = (num(f[1]) as u32, num(f[2]) as u32, num(f[3]));
                e.set_source(Source { id: 1, charged: (0xc10000..0xc14000).contains(&off) });
                while e.write(off, size * 8, val) == Write::Busy {
                    settle(&mut e);
                }
                e.run(usize::MAX);
            }
            Some("read") => {
                let (size, off) = (num(f[1]) as u32, num(f[2]) as u32);
                for k in 0..size / 4 {
                    loop {
                        match e.read(off + 4 * k, 32) {
                            Read::Value(_) => break,
                            Read::Busy => settle(&mut e),
                            Read::Fault => panic!("line {}: the pipe faulted: {:?}", n + 1, e.fault()),
                        }
                    }
                }
            }
            _ => continue,
        }
        if n % FRAME_RECORDS == 0 {
            compare(&mut e, &mut shown, n);
        }
    }
    compare(&mut e, &mut shown, usize::MAX);
    eprintln!("menu: {frames} frames, rows composed {full} whole, {named} marked");
    assert!(frames > 50, "the menu draws over many frames");
    assert!(named * 4 < full, "the menu's text marks a part of the picture: {named} of {full}");
}
