//! A compose of only the rows the damage marks name shows what a compose of the whole frame does.

mod support;

use sgi_engine::{BoardEngine, Mark, Write, DAMAGE_ANY};
use sgi_mgras::dev::mgras::engine::{Mode, Scan, ScanSource, DAMAGE_DISPLAY};
use sgi_mgras::dev::mgras::hq3::host;
use support::*;

/// Steps of a recording taken as one frame.
const FRAME_STEPS: usize = 64;

/// A store of `data` to the display bus: `width` bytes, to select `crs` of device `dev`.
fn bus<E: BoardEngine>(e: &mut E, dev: u32, crs: u32, width: u32, data: u32) {
    let off = 0x60000 + dev * 0x400 + (crs << 7) + ((width & 3) << 3);
    assert_eq!(e.write(off, 32, (data as u64) << (8 * (4 - width))), Write::Done);
}

/// Show the 24-bit RGB planes of buffer 0x240, whatever a window's ID.
fn show_rgb<E: BoardEngine>(e: &mut E) {
    for did in 0..32 {
        bus(e, 7, 1, 1, did);
        bus(e, 7, 4, 4, 4);
    }
    bus(e, 7, 1, 1, 0);
    bus(e, 7, 6, 4, 0x240);
}

/// The display rows a mark names, as the shell maps them: a draw's rows count from the screen's bottom.
fn rows_of(m: &Mark, h: usize, rows: &mut [bool]) {
    let (y0, y1) = (m.rect.y0 as i64, m.rect.y1 as i64);
    let (first, last) = if m.store == DAMAGE_DISPLAY { (y0, y1) } else { (h as i64 - 1 - y1, h as i64 - 1 - y0) };
    for (y, r) in rows.iter_mut().enumerate() {
        *r |= m.store == DAMAGE_ANY || (first..=last).contains(&(y as i64));
    }
}

/// Compose the rows `rows` names into `shown`, a run at a time.
fn recompose(src: &ScanSource<Mem>, rows: &[bool], shown: &mut [u32]) {
    let mut y = 0;
    while y < rows.len() {
        let end = (y..rows.len()).find(|r| !rows[*r]).unwrap_or(rows.len());
        if end > y {
            assert!(matches!(src.rows(y, end, shown), Ok(Scan::Done(..))));
        }
        y = end + 1;
    }
}

fn raster(e: &mut impl BoardEngine, reg: u32, val: u32, exec: bool) {
    put(e, host::RASTER + reg * 4 + if exec { 0x1000 } else { 0 }, 32, val as u64);
}

/// Per recording: frames with marks, rows a whole-frame compose of them costs, rows of the marks.
fn check(name: &str, text: &str) -> (usize, usize, usize) {
    let rep = parse(text);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    show_rgb(&mut p.e);
    let src = p.e.scan_source().unwrap();
    let mut shown = vec![0u32; 2048 * 2048];
    let mut full = Vec::new();
    let (w, h) = p.e.scanout(0, &mut full).unwrap().unwrap();
    let (w, h) = (w as usize, h as usize);
    assert!(matches!(src.rows(0, h, &mut shown), Ok(Scan::Done(..))));
    p.fresh.clear();
    let (mut frames, mut whole, mut marked) = (0, 0, 0);
    let mut rows = vec![false; h];
    for (n, c) in rep.cmds.iter().enumerate() {
        p.apply(c);
        p.fresh.drain(..).for_each(|m| rows_of(&m, h, &mut rows));
        if (n + 1) % FRAME_STEPS != 0 && n + 1 != rep.cmds.len() {
            continue;
        }
        recompose(&src, &rows, &mut shown);
        let n_rows = rows.iter().filter(|r| **r).count();
        p.e.scanout(0, &mut full).unwrap();
        for y in 0..h {
            assert!(shown[y * w..(y + 1) * w] == full[y * w..(y + 1) * w], "{name}: row {y} differs after step {n}, with {n_rows} rows marked");
        }
        (frames, whole, marked) = (frames + (n_rows > 0) as usize, whole + if n_rows > 0 { h } else { 0 }, marked + n_rows);
        rows.fill(false);
    }
    (frames, whole, marked)
}

#[test]
fn composing_the_marked_rows_gives_the_frame_a_whole_compose_does() {
    let (mut whole, mut marked) = (0, 0);
    for (name, text) in REPLAYS {
        let (frames, w, m) = check(name, text);
        eprintln!("{name}: {frames} frames, rows composed {w} whole, {m} marked");
        (whole, marked) = (whole + w, marked + m);
    }
    eprintln!("all: rows composed {whole} whole, {marked} marked");
    assert!(marked < whole, "a draw marks fewer rows than the frame");
}

/// Draw with `draw` once the recording has played, mark what it posts and compare with a whole-frame compose.
fn drawn_after(text: &str, draw: impl FnOnce(&mut Player)) -> (usize, usize) {
    let rep = parse(text);
    let mut p = Player::new(config(&rep.head, Mode::Inline, None), Drive::Each);
    show_rgb(&mut p.e);
    rep.cmds.iter().for_each(|c| p.apply(c));
    let src = p.e.scan_source().unwrap();
    let mut shown = vec![0u32; 2048 * 2048];
    let mut full = Vec::new();
    let (w, h) = p.e.scanout(0, &mut full).unwrap().unwrap();
    let (w, h) = (w as usize, h as usize);
    recompose(&src, &vec![true; h], &mut shown);
    p.fresh.clear();
    draw(&mut p);
    drive(&mut p.e);
    // An empty host-memory write, to collect the marks the draw posted.
    p.apply(&Cmd::Dma { addr: 0, bytes: Vec::new() });
    let mut rows = vec![false; h];
    p.fresh.drain(..).for_each(|m| rows_of(&m, h, &mut rows));
    recompose(&src, &rows, &mut shown);
    p.e.scanout(0, &mut full).unwrap();
    let stale = (0..h).filter(|y| shown[y * w..(y + 1) * w] != full[y * w..(y + 1) * w]).count();
    (stale, rows.iter().filter(|r| **r).count())
}

#[test]
fn a_new_plane_layout_marks_every_row() {
    for name in ["gl-swap-tmesh-sphere", "root-icon"] {
        let text = REPLAYS.iter().find(|r| r.0 == name).unwrap().1;
        // The layout alone, then a small block in buffer 0x240, which is all a draw marks.
        let (stale, rows) = drawn_after(text, |p| {
            raster(&mut p.e, 0x16E, 0x0E, false);
            for (r, v, x) in [(0x16d, 0x240, false), (0x161, 0x0c00_6300, false), (0x110, 0, false), (0x05b, 0x33_2211, false), (0x046, 0x000a_000a, false), (0x047, 0x000d_000d, false), (0x045, 0x18, true)] {
                raster(&mut p.e, r, v, x);
            }
        });
        assert_eq!(stale, 0, "{name}: rows left in the old layout, of {rows} marked");
    }
}

/// A PIO transfer of two lines of RGBA pixels, which takes the plain line path.
#[test]
fn a_pio_transfer_marks_the_rows_it_stored_in() {
    let text = REPLAYS.iter().find(|r| r.0 == "fill").unwrap().1;
    let (stale, rows) = drawn_after(text, |p| {
        for (r, v, x) in [(0x112, 0xCAC, false), (0x115, 1023 << 16, false), (0x16D, 0x240, false), (0x163, u32::MAX, false), (0x161, 0x0C00_6204, false), (0x110, 3 << 22, false), (0x159, 0xC0_0080, false), (0x153, 2 << 16 | 4, false)] {
            raster(&mut p.e, r, v, x);
        }
        for (r, v, x) in [(0x045, 0x18, false), (0x046, 100 << 16 | 600, false), (0x047, 103 << 16 | 601, true)] {
            raster(&mut p.e, r, v, x);
        }
        // Two doublewords a line, four pixels of four bytes.
        for dw in 0..4u32 {
            raster(&mut p.e, 0x070, 0x1122_3344 + dw, false);
            raster(&mut p.e, 0x071, 0x5566_7788 + dw, true);
        }
    });
    assert_eq!((stale, rows), (0, 32), "marked rows");
}

/// Past 16 waiting marks the outbox folds them into one DAMAGE_ANY whose box joins a draw's rows (from the
/// bottom) with the cursor's (from the top), so a shell has to treat it as everything, not map it as rows.
#[test]
fn many_marks_fold_into_one_for_everything_with_rows_of_both_kinds() {
    let mut p = Player::new(config("sgi-mgras", Mode::Inline, None), Drive::Each);
    for (reg, val) in [(0x1d, 3), (3, 200 + 31)] {
        bus(&mut p.e, 8, 0, 3, reg << 16 | val);
    }
    p.apply(&Cmd::Dma { addr: 0, bytes: Vec::new() });
    p.fresh.clear();
    bus(&mut p.e, 8, 0, 3, 3 << 16 | (300 + 31));
    for (r, v) in [(0x161, 0x0c00_6300), (0x110, 0), (0x05b, 0x33_2211)] {
        raster(&mut p.e, r, v, false);
    }
    for page in 0..16 {
        raster(&mut p.e, 0x16d, 0x100 + page, false);
        raster(&mut p.e, 0x046, 0x000a_000a, false);
        raster(&mut p.e, 0x047, 0x000d_000d, false);
        raster(&mut p.e, 0x045, 0x18, true);
    }
    drive(&mut p.e);
    p.apply(&Cmd::Dma { addr: 0, bytes: Vec::new() });
    let [m] = p.fresh[..] else { panic!("{:?}", p.fresh) };
    assert_eq!(m.store, DAMAGE_ANY);
    assert!(m.rect.y0 <= 10 && m.rect.y1 >= 331, "{:?}", m.rect);
}
