//! The raster scenes of the Octane's standalone diagnostic, replayed through
//! the HQ3 command words it sends. The diagnostic compares what it reads back
//! with a table stored in its binary; the stored value that survives here is
//! the wrapping sum of the table's words (the diagnostic's own check value).
//! The tables themselves are SGI's and are not kept.
//!
//! Source of the register streams and the sums: the Octane diagnostic notes
//! in the gr4 reverse-engineering repository. Every register value is what the
//! diagnostic writes, a software expectation, not a measurement of a board.

mod support;

use sgi_mgras::dev::mgras::engine::{Board, MgrasEngine, Mode};
use sgi_mgras::dev::mgras::pixmem::{Buffer, Kind};
use sgi_mgras::wgpu::{Gpu, GpuOptions};
use std::sync::Arc;
use sgi_engine::BoardEngine;
use support::*;

/// A raster write as the diagnostic sends it: header, then the value.
fn reg(w: &mut Vec<u32>, r: u32, v: u32) {
    w.push(((r + 0x1800) & 0x1fff) << 8 | 0x400004);
    w.push(v);
}

fn regs(w: &mut Vec<u32>, list: &[(u32, u32)]) {
    list.iter().for_each(|&(r, v)| reg(w, r, v));
}

/// A window coordinate as the diagnostic writes it: float32(x + 49151.5).
fn coord(x: f64) -> u32 {
    ((x + 49151.5) as f32).to_bits()
}

/// The pixel-processor defaults the diagnostic sets first.
const INIT: &[(u32, u32)] = &[
    (0x112, 0x4), (0x160, 0), (0x161, 0x4200), (0x162, 0xff), (0x163, 0xffff_ffff), (0x164, 0xffff_ffff), (0x165, 0x5884_0000),
    (0x166, 0x7), (0x167, 0xffff), (0x168, 0x0fff_fff1), (0x169, 0x7), (0x16a, 0), (0x16d, 0x000c_8240), (0x16e, 0x25e), (0x16f, 0),
    (0x68, 0), (0x69, 0), (0x176, 0x01a0_8190), (0x177, 0x0008_07f8), (0x178, 0x0008_07f8), (0x179, 0xfff0_0ff0), (0x170, 0x0008_1a19),
    (0x171, 0x0008_807f), (0x172, 0x0008_807f), (0x173, 0xff), (0x17b, 0),
];

/// The clear before the green and blue triangles: a colour, then zero.
fn clears(w: &mut Vec<u32>) {
    let tail = [(0x110, 0x10_0000), (0x161, 0x4203), (0x46, 0), (0x47, 0x0540_0400), (0x445, 0x18), (0x110, 0x5000), (0x161, 0x4203)];
    regs(w, &[(0x176, 0x0340_0330), (0x177, 0x0006_7660), (0x178, 0x000c_dcc0), (0x179, 0x1991_a198), (0x170, 0x3433), (0x171, 0x6766), (0x172, 0xcdcc), (0x173, 0x0008_1a19)]);
    regs(w, &tail);
    regs(w, &[(0x176, 0x10_0000), (0x177, 0x1000), (0x178, 0x1000), (0x179, 0x1000), (0x170, 0x100), (0x171, 0x100), (0x172, 0x100), (0x173, 0x100)]);
    regs(w, &tail);
}

/// The triangle (0,15), (23,7), (0,0) with its colour registers.
fn triangle(w: &mut Vec<u32>, colour: &[(u32, u32)]) {
    regs(w, &[(0x4, 0x4740_0680), (0x5, 0x473f_ff80), (0x6, 0), (0x7, 0), (0x8, 0), (0x9, 0x02e0_0000), (0xa, 0xffff_ffff), (0xb, 0xfcb6_db6c)]);
    regs(w, &[(0x0, 0x473f_ff80), (0x1, 0x473f_ff80), (0x2, 0x4740_1680), (0x3, 0x4740_0e80)]);
    regs(w, &(0x5c..=0x67).map(|r| (r, colour.iter().find(|c| c.0 == r).map_or(0, |c| c.1))).collect::<Vec<_>>());
    reg(w, 0x445, 0);
}

const RED: &[(u32, u32)] = &[(0x5c, 0x00ff_f000), (0x5f, 0x00ff_f000), (0x60, 0xffee_f000), (0x61, 0x0000_5ef4)];
const GREEN: &[(u32, u32)] = &[(0x5f, 0x00ff_f000), (0x62, 0x0011_1000), (0x63, 0xffff_a10c)];
const BLUE: &[(u32, u32)] = &[(0x5e, 0x007f_f800), (0x5f, 0x00ff_f000), (0x64, 0xfff7_7800), (0x65, 0x0008_87ff)];

/// One line of the five: end then start vertex, the increments, and a colour.
/// Stippled lines write the repeat word to 0x15b and the pattern to 0x15a.
/// An integer line (the diagnostic's argument 7) skips the float endpoints
/// and the step registers 0x10 and 0x12 and writes the truncated start and
/// end to 0x40 and 0x41, x in the high half.
fn line(w: &mut Vec<u32>, end: (f64, f64), start: (f64, f64), inc: (u32, u32), colour: &[(u32, u32)], stipple: bool, smooth: bool, int: bool) {
    line_with(w, end, start, inc, colour, stipple, smooth, int, &|i| 0xff - 4 * i);
}

/// `line` with the smooth line's filter table (weight at index i) as given.
fn line_with(w: &mut Vec<u32>, end: (f64, f64), start: (f64, f64), inc: (u32, u32), colour: &[(u32, u32)], stipple: bool, smooth: bool, int: bool, filter: &dyn Fn(u32) -> u32) {
    if int {
        let pack = |p: (f64, f64)| (p.0 as u32) << 16 | p.1 as u32;
        regs(w, &[(0x11, inc.1), (0x40, pack(start)), (0x41, pack(end))]);
    } else {
        regs(w, &[(0xc, coord(end.0)), (0xd, coord(end.1)), (0xe, coord(start.0)), (0xf, coord(start.1)), (0x10, inc.0), (0x11, inc.1), (0x12, 0)]);
    }
    reg(w, 0x110, 0x7000);
    if stipple {
        regs(w, &[(0x110, 0x70a0), (0x15b, 0x000f_0000), (0x15a, 0xa987_6543)]);
    }
    regs(w, &[(0x60, 0), (0x62, 0), (0x64, 0), (0x66, 0)]);
    regs(w, &(0x5c..=0x5f).map(|r| (r, colour.iter().find(|c| c.0 == r).map_or(0, |c| c.1))).collect::<Vec<_>>());
    reg(w, 0x116, 0);
    if smooth {
        reg(w, 0x15c, 0xb000_0000);
        (0..2).for_each(|_| (0..64).for_each(|i| reg(w, 0x15d, filter(i))));
        reg(w, 0x15c, 0xb000_0080);
        (0..32).for_each(|_| reg(w, 0x15d, 0x100));
        // The fill mode's written bits are 1:0 and 3; the rest is the register's
        // earlier value, so the 8-8-8-8 type of 0x4200 stays (inferred).
        regs(w, &[(0x110, 0x7100), (0x165, 0x8000_0074), (0x165, 0x7880_c8c4), (0x161, 0x0c00_420b), (0x116, 1)]);
        reg(w, 0x445, 0x63);
    } else {
        reg(w, 0x445, 3);
    }
}

fn lines(w: &mut Vec<u32>) {
    let (a, b) = ((0x40_0000, 0x100_0000), (0x100_0000, 0));
    line(w, (0.0, 0.0), (4.0, 16.0), a, &[(0x5c, 0xff_f000), (0x5f, 0xff_f000)], false, false, false);
    line(w, (5.0, 0.0), (9.0, 16.0), a, &[(0x5d, 0xff_f000), (0x5f, 0xff_f000)], false, true, false);
    line(w, (0.0, 5.0), (5.0, 5.0), b, &[(0x5e, 0xff_f000), (0x5f, 0xff_f000)], true, false, false);
    line(w, (5.0, 5.0), (10.0, 5.0), b, &[(0x5e, 0xff_f000), (0x5f, 0xff_f000)], true, false, false);
    line(w, (12.0, 0.0), (12.0, 10.0), (0, 0x100_0000), &[(0x5c, 0x7f_f800), (0x5e, 0x7f_f800), (0x5f, 0xff_f000)], false, false, true);
}

/// Send the words, then read `rows` by `cols` back from (0, 0) of the draw page.
fn readback(board: Board, mode: Mode, jit: Option<u32>, words: &[u32], rows: u32, cols: u32) -> Vec<u32> {
    readback_on(board, mode, jit, None, words, rows, cols)
}

fn readback_on(board: Board, mode: Mode, jit: Option<u32>, gpu: Option<&Arc<Gpu>>, words: &[u32], rows: u32, cols: u32) -> Vec<u32> {
    let mut cfg = config("sgi-mgras", mode, jit);
    cfg.board = board;
    cfg.octane = false;
    cfg.queue_ops = 1 << 20;
    let mut e = MgrasEngine::new(cfg, Mem::default(), gpu, None).unwrap();
    for chunk in words.chunks(4096) {
        assert_eq!(e.push_words(0, chunk), chunk.len());
        drive(&mut e);
    }
    let drb = e.raster_reg(0x16e);
    let b = Buffer::new(0x240, Kind::Wide, drb);
    (0..rows).flat_map(|y| (0..cols).map(move |x| (x, y))).map(|(x, y)| e.pixel(&b, x, y) as u32).collect()
}

fn sum(px: &[u32]) -> u32 {
    px.iter().fold(0u32, |a, &p| a.wrapping_add(p))
}

fn covered(px: &[u32]) -> usize {
    px.iter().filter(|&&p| p & 0xFF_FFFF != 0).count()
}

fn scene(f: impl Fn(&mut Vec<u32>)) -> Vec<u32> {
    let mut w = Vec::new();
    regs(&mut w, INIT);
    f(&mut w);
    w
}

/// The scenes: name, words, rows, columns read back, and the diagnostic's
/// stored sum.
fn scenes() -> Vec<(&'static str, Vec<u32>, u32, u32, u32)> {
    vec![
        ("red triangle", scene(|w| triangle(w, RED)), 16, 24, 0x5400_5595),
        ("green triangle", scene(|w| { clears(w); triangle(w, GREEN) }), 16, 24, 0x5455_be00),
        ("blue triangle", scene(|w| { clears(w); triangle(w, BLUE) }), 16, 24, 0xa718_0000),
        ("five lines", scene(lines), 17, 14, 0xc584_ed6b),
    ]
}

/// What the stored tables settle without being kept: each triangle's table
/// has 172 covered pixels.
#[test]
fn the_triangles_cover_as_many_pixels_as_the_stored_tables() {
    for (name, w, rows, cols, _) in scenes().into_iter().take(3) {
        assert_eq!(covered(&readback(Board::High, Mode::Inline, None, &w, rows, cols)), 172, "{name}");
    }
}

/// Row r of a stored table is the triangle at y = r + 1: the right edge
/// there gives these pixel counts (gr4 ide-raster-tests.md), 14 rows.
#[test]
fn the_triangles_have_the_stored_row_counts() {
    const ROWS: [usize; 16] = [3, 7, 10, 13, 16, 20, 23, 20, 17, 14, 11, 9, 6, 3, 0, 0];
    for (name, w, rows, cols, _) in scenes().into_iter().take(3) {
        let px = readback(Board::High, Mode::Inline, None, &w, rows, cols);
        let got: Vec<usize> = px.chunks(cols as usize).map(covered).collect();
        assert_eq!(got, ROWS, "{name}");
    }
}

/// A GPU unit, or None (the tests then skip that path; `IRIS_REQUIRE_GPU=1`
/// makes a missing one an error).
fn gpu() -> Option<Arc<Gpu>> {
    match Gpu::new(&GpuOptions::default(), Box::new(|m| panic!("GPU failed: {m}"))) {
        Ok(g) => {
            eprintln!("GPU path on {} ({:?}, {:?})", g.caps.adapter, g.caps.backend, g.caps.device_type);
            Some(g)
        }
        Err(e) => {
            if std::env::var_os("IRIS_REQUIRE_GPU").is_some_and(|v| v == "1") {
                panic!("no GPU: {e}");
            }
            eprintln!("skipped the GPU path: {e}");
            None
        }
    }
}

/// Every path a scene runs on: name and the words it stores. The
/// interpreter first, then the threaded engine, the JIT and the GPU unit.
fn paths(w: &[u32], rows: u32, cols: u32, gpu: Option<&Arc<Gpu>>) -> Vec<(&'static str, Vec<u32>)> {
    let mut out = vec![("interpreter", readback(Board::High, Mode::Inline, None, w, rows, cols))];
    out.push(("threaded", readback(Board::High, Mode::Threaded, None, w, rows, cols)));
    #[cfg(feature = "gr4-jit")]
    out.push(("JIT", readback(Board::High, Mode::Inline, Some(sgi_mgras::dev::mgras::rss_jit::MODE_SYNC), w, rows, cols)));
    if let Some(g) = gpu {
        out.push(("GPU", readback_on(Board::High, Mode::Inline, None, Some(g), w, rows, cols)));
    }
    out
}

/// Inline, threaded, JIT and GPU runs store the same scene.
#[test]
fn every_path_stores_the_same_scene() {
    let g = gpu();
    for (name, w, rows, cols, _) in scenes() {
        let all = paths(&w, rows, cols, g.as_ref());
        for (path, got) in &all[1..] {
            assert_eq!(got, &all[0].1, "{name}: {path}");
        }
    }
}

/// Lines the diagnostic never draws, in its register interface: smooth,
/// stippled, integer and blended ones at pseudo-random places over one
/// 48 by 40 window, so the JIT's and the GPU unit's words are compared on
/// more than the five.
fn random_lines(w: &mut Vec<u32>) {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    let colours: [&[(u32, u32)]; 3] = [&[(0x5c, 0xff_f000), (0x5f, 0xff_f000)], &[(0x5d, 0xff_f000), (0x5e, 0x7f_f800), (0x5f, 0xff_f000)], &[(0x5c, 0x3f_f000), (0x5d, 0xbf_f000), (0x5f, 0x7f_f800)]];
    for _ in 0..60 {
        let p = |n: &mut dyn FnMut(u64) -> u64| (n(46) as f64 + 1.0 + n(4) as f64 / 4.0, n(38) as f64 + 1.0 + n(4) as f64 / 4.0);
        let (a, b) = (p(&mut next), p(&mut next));
        let int = next(4) == 0;
        let (a, b) = if int { ((a.0.trunc(), a.1.trunc()), (b.0.trunc(), b.1.trunc())) } else { (a, b) };
        let (dx, dy) = ((b.0 - a.0).abs(), (b.1 - a.1).abs());
        let inc = if dy >= dx { ((dx / dy.max(1e-6) * 16_777_216.0) as u32, 0x100_0000) } else { (0x100_0000, (dy / dx.max(1e-6) * 16_777_216.0) as u32) };
        let stipple = next(3) == 0;
        let smooth = next(2) == 0;
        line(w, a, b, inc, colours[next(3) as usize], stipple, smooth, int);
    }
}

/// The same for random diagnostic lines.
#[test]
fn every_path_stores_the_same_random_lines() {
    let g = gpu();
    let w = scene(random_lines);
    let all = paths(&w, 40, 48, g.as_ref());
    assert!(covered(&all[0].1) > 100, "the random lines drew little");
    for (path, got) in &all[1..] {
        assert_eq!(got, &all[0].1, "random lines: {path}");
    }
}

/// The acceptance the plan names: each scene's wrapping sum is the
/// diagnostic's stored sum. The three triangles meet it with the classic
/// Bayer matrix (inferred); the lines are the open acceptance below.
#[test]
fn the_engine_stores_what_the_diagnostic_stores() {
    for (name, w, rows, cols, want) in scenes().into_iter().take(3) {
        let got = sum(&readback(Board::High, Mode::Inline, None, &w, rows, cols));
        assert_eq!(got, want, "{name}: sum {got:#010x}, stored {want:#010x}");
    }
}

/// What gr4 reads from the stored lines table without keeping it: the
/// steep red line moves one column every four rows, the smooth green line
/// covers one or two pixels a row around column 5 + row / 4, the stippled
/// blue lines lie on row 5, and the purple vertical line at column 12
/// alternates 0x80 and 0x7f by row (127.5 dithered).
#[test]
fn the_lines_draw_the_stored_tables_steps() {
    let px = readback(Board::High, Mode::Inline, None, &scene(lines), 17, 14);
    let at = |r: usize, c: usize| px[r * 14 + c];
    // The two stippled blue lines share row 5 and cover some of its pixels.
    let blue = (0..14).filter(|&c| at(5, c) & 0xFFFF_00FF == 0xffff_0000).count();
    assert!((1..10).contains(&blue), "blue dots on row 5: {blue}");
    for r in (0..16).filter(|&r| r != 5) {
        assert_eq!(at(r, r / 4), 0xff00_00ff, "red, row {r}");
        // Green weights fall off with the pixel centre's distance from x = 5 + r / 4.
        let green: Vec<usize> = (0..14).filter(|&c| at(r, c) != 0 && at(r, c) & 0x00FF_00FF == 0).collect();
        let on_centre = r % 4 == 2;
        assert_eq!(green.len(), if on_centre { 1 } else { 2 }, "green pixels, row {r}");
        assert!(green.iter().all(|&c| (5 + r / 4 - 1..=5 + r / 4 + 1).contains(&c)), "green columns {green:?}, row {r}");
    }
    assert_eq!(at(2, 5), 0xff00_ff00, "a line through a pixel centre is full weight");
    // The blue dot at (5, 5) blends over the green line's pixel: 63 * (255 - 16) / 255.
    assert_eq!((at(5, 5) >> 8) & 0xFF, 0x3b, "blue over green");
    for r in 0..=10 {
        let want = if r % 2 == 0 { 0x80 } else { 0x7f };
        assert_eq!(at(r, 12), 0xff00_0000 | want << 16 | want, "purple, row {r}");
    }
}

/// The smooth line's weights come from the table the loader writes through
/// the stepping indirect address: halved weights halve the green of a pixel
/// the line passes through the centre of.
#[test]
fn the_smooth_line_reads_the_loaded_table() {
    let at_centre = |filter: &dyn Fn(u32) -> u32| {
        let w = scene(|w| line_with(w, (5.0, 0.0), (9.0, 16.0), (0x40_0000, 0x100_0000), &[(0x5d, 0xff_f000), (0x5f, 0xff_f000)], false, true, false, filter));
        readback(Board::High, Mode::Inline, None, &w, 3, 8)[2 * 8 + 5]
    };
    assert_eq!(at_centre(&|i| 0xff - 4 * i), 0xff00_ff00);
    let half = at_centre(&|i| (0xff - 4 * i) / 2);
    assert!((0x7d..=0x7f).contains(&((half >> 8) & 0xFF)), "green {half:#010x}");
}

/// The lines meet the stored sum.
#[test]
fn the_engine_stores_the_diagnostics_lines() {
    let got = sum(&readback(Board::High, Mode::Inline, None, &scene(lines), 17, 14));
    assert_eq!(got, 0xc584_ed6b, "five lines: sum {got:#010x}");
}

/// The shipped diagnostic holds the stored tables; this reads them in place
/// from a local copy and never keeps a word. Offsets: data base 0x20953380
/// is file offset 0x143380 (red, green, blue at +0, +0x600, +0xc00), the
/// lines table is at 0x144b80. Words are big-endian 0xAABBGGRR.
const IDE_ENV: &str = "IRIS_MGRAS_IDE";
const IDE_SIZE: u64 = 1_516_224;

fn stored(ide: &[u8], offset: usize, n: usize) -> Vec<u32> {
    ide[offset..offset + 4 * n].chunks(4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).collect()
}

/// Words of the engine's scene that differ from the stored table's.
fn differing(name: &str, got: &[u32], want: &[u32], cols: usize) -> usize {
    let bad: Vec<usize> = (0..want.len()).filter(|&i| got[i] != want[i]).collect();
    println!("{name}: {} of {} words differ (sum {:#010x}, stored {:#010x})", bad.len(), want.len(), sum(got), sum(want));
    bad.iter().take(8).for_each(|&i| println!("  row {} col {}: engine {:#010x}", i / cols, i % cols, got[i]));
    bad.len()
}

/// Word for word against the stored tables, when the shipped binary is
/// available locally, on every path: the interpreter, the threaded engine,
/// the JIT and the GPU unit must each match all four tables exactly.
#[test]
fn the_engine_matches_the_stored_tables_word_for_word() {
    let Ok(path) = std::env::var(IDE_ENV) else {
        eprintln!("skipped: {IDE_ENV} names no diagnostic binary");
        return;
    };
    let Ok(ide) = std::fs::read(&path) else {
        eprintln!("skipped: no diagnostic binary at {path} (set {IDE_ENV})");
        return;
    };
    assert_eq!(ide.len() as u64, IDE_SIZE, "{path} is not the IRIX 6.4 Octane diagnostic");
    let g = gpu();
    let tables = [(0x143380, 0), (0x143980, 1), (0x143f80, 2), (0x144b80, 3)];
    let mut bad = 0;
    for (offset, i) in tables {
        let (name, w, rows, cols, _) = scenes().swap_remove(i);
        let want = stored(&ide, offset, (rows * cols) as usize);
        for (path, got) in paths(&w, rows, cols, g.as_ref()) {
            bad += differing(&format!("{name}, {path}"), &got, &want, cols as usize);
        }
    }
    assert_eq!(bad, 0, "{bad} words differ from the stored tables");
}
