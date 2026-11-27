//! Drawing marks the rows it wrote, and composing only the marked rows shows what a whole compose does.

mod support;

use sgi_engine::{BoardEngine, Chunk, VecSource};
use sgi_gr2::dev::gr2::re3::*;
use sgi_gr2::dev::gr2::rows::RowSet;
use sgi_gr2::dev::gr2::{hq2, AuxWindow, Gr2Probe, Gr2Variant};
use support::*;

// ---- the RE3 alone: each op marks the VRAM rows it wrote ----

fn bare() -> Box<Re3> {
    // SAFETY: Re3 is plain data and valid when zeroed.
    let mut r = unsafe { Box::<Re3>::new_zeroed().assume_init() };
    for (reg, v) in [(RE3_LOGIC_OP, ROP_COPY), (RE3_COLOR_PLANE_MASK, 0x00ff_ffff), (RE3_CLIP_X_HI, FB_W as u32 - 1), (RE3_CLIP_Y_HI, FB_H as u32 - 1)] {
        r.write_reg(reg, v);
    }
    r
}

/// Run `f`; the VRAM rows it changed and the rows it marked, each rows in order.
fn run(r: &mut Re3, f: impl FnOnce(&mut Re3)) -> (Vec<usize>, Vec<usize>) {
    let before = r.vram.to_vec();
    r.rows = RowSet::default();
    f(r);
    let changed = (0..FB_H).filter(|&y| r.vram[y * FB_W..][..FB_W] != before[y * FB_W..][..FB_W]).collect();
    (changed, (0..FB_H).filter(|&y| r.rows.has(y)).collect())
}

/// Every row that changed is marked, and the marks are `want`.
#[track_caller]
fn marks(got: (Vec<usize>, Vec<usize>), want: std::ops::RangeInclusive<usize>) {
    let (changed, marked) = got;
    assert_eq!(marked, want.collect::<Vec<_>>());
    assert!(changed.iter().all(|y| marked.contains(y)), "rows {changed:?} changed, {marked:?} marked");
}

#[track_caller]
fn marks_none(got: (Vec<usize>, Vec<usize>)) {
    assert_eq!(got, (vec![], vec![]));
}

fn flat(r: &mut Re3, x: u32, y: u32, n: u32) {
    r.write_reg(RE3_RED_START, 0x42 << 11);
    r.write_reg(RE3_CUR_X, x);
    r.write_reg(RE3_CUR_Y, y);
    r.write_reg(RE3_SPAN_LEN, n);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_FLAT_5);
}

/// A shaded span of `n` pixels from (3, 20) stepping `dy` (1.0 is 1 << 14) a pixel.
fn shaded(r: &mut Re3, n: u32, dy: u32) {
    r.write_reg(RE3_RED_START, 0x42 << 11);
    r.write_reg(RE3_X_STEP, 0);
    r.write_reg(RE3_Y_STEP, dy);
    r.write_reg(RE3_CUR_X, 3);
    r.write_reg(RE3_CUR_Y, 20);
    r.write_reg(RE3_SPAN_LEN, n);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_SMOOTH);
}

#[test]
fn a_flat_span_marks_its_row() {
    let mut r = bare();
    marks(run(&mut r, |r| flat(r, 100, 10, 7)), 10..=10);
    marks_none(run(&mut r, |r| flat(r, 100, 11, 0)));
}

#[test]
fn a_span_marks_only_rows_the_scissor_lets_through() {
    let mut r = bare();
    r.write_reg(RE3_CLIP_Y_LO, 50);
    r.write_reg(RE3_CLIP_Y_HI, 60);
    marks_none(run(&mut r, |r| flat(r, 0, 10, 5)));
    marks(run(&mut r, |r| flat(r, 0, 55, 5)), 55..=55);
    // A span off the frame buffer does not wrap into a row.
    r.write_reg(RE3_CLIP_Y_HI, FB_H as u32 + 100);
    marks_none(run(&mut r, |r| flat(r, 0, FB_H as u32 + 5, 5)));
}

#[test]
fn a_shaded_span_marks_the_rows_its_y_step_walks() {
    let mut r = bare();
    marks(run(&mut r, |r| shaded(r, 5, 1 << 14)), 20..=24);
    marks(run(&mut r, |r| shaded(r, 5, 0xc000)), 16..=20);
    marks(run(&mut r, |r| shaded(r, 5, 1 << 13)), 20..=22);
    marks_none(run(&mut r, |r| shaded(r, 0, 1 << 14)));
    // The scissor cuts the walk.
    r.write_reg(RE3_CLIP_Y_HI, 22);
    marks(run(&mut r, |r| shaded(r, 5, 1 << 14)), 20..=22);
}

/// HOST_DATA stores `n` pixels down a column from (4, 3).
fn host_write(r: &mut Re3, n: u32, plane: u32) {
    r.write_reg(RE3_HOST_PLANE, plane);
    r.write_reg(RE3_X_STEP, 0);
    r.write_reg(RE3_Y_STEP, 1 << 14);
    r.write_reg(RE3_CUR_X, 4);
    r.write_reg(RE3_CUR_Y, 3);
    r.write_reg(RE3_SPAN_LEN, n);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_HOST_WRITE);
    for _ in 0..n {
        r.write_reg(RE3_HOST_DATA, 0x77);
    }
}

#[test]
fn a_host_write_marks_the_rows_it_stored_to() {
    let mut r = bare();
    marks(run(&mut r, |r| host_write(r, 3, HPLANE_COLOR)), 3..=5);
    // The window test turns pixels away.
    r.write_reg(RE3_WID_TEST_ON, 1);
    r.write_reg(RE3_WINDOW_ID, 1);
    marks_none(run(&mut r, |r| host_write(r, 3, HPLANE_COLOR)));
    r.write_reg(RE3_WID_TEST_ON, 0);
    // The Z planes are not shown.
    let (changed, marked) = run(&mut r, |r| host_write(r, 3, HPLANE_Z));
    assert_eq!((changed, marked), (vec![], vec![]));
    assert!(r.zbuf.iter().any(|&z| z == 0x77), "the Z plane took the store");
}

#[test]
fn a_host_write_with_several_pixels_a_word_marks_them_all() {
    let mut r = bare();
    r.write_reg(RE3_UNPACK_MODE, 3);
    r.write_reg(RE3_HOST_PLANE, HPLANE_COLOR);
    r.write_reg(RE3_X_STEP, 0);
    r.write_reg(RE3_Y_STEP, 1 << 14);
    r.write_reg(RE3_CUR_X, 4);
    r.write_reg(RE3_CUR_Y, 3);
    r.write_reg(RE3_SPAN_LEN, 4);
    r.write_reg(RE3_DRAW_TRIGGER, DRAW_HOST_WRITE);
    marks(run(&mut r, |r| { r.write_reg(RE3_HOST_DATA, 0x0102_0304); }), 3..=6);
}

fn column(r: &mut Re3, rows: std::ops::Range<usize>) {
    for y in rows {
        r.vram[y * FB_W] = y as u32 + 1;
    }
}

#[test]
fn a_copy_marks_the_destination_rows_not_the_source() {
    let mut r = bare();
    column(&mut r, 0..8);
    marks(run(&mut r, |r| r.copy_rect(0, 0, 1, 6, 0, 2)), 2..=7);
    marks(run(&mut r, |r| r.copy_rect(0, 2, 1, 6, 0, 0)), 0..=5);
}

#[test]
fn a_copy_marks_only_the_rows_on_the_frame_buffer_and_through_the_scissor() {
    let mut r = bare();
    column(&mut r, 0..8);
    let top = FB_H as i32 - 2;
    marks(run(&mut r, |r| r.copy_rect(0, 0, 1, 6, 0, top)), top as usize..=FB_H - 1);
    r.write_reg(RE3_CLIP_Y_HI, 3);
    marks(run(&mut r, |r| r.copy_rect(0, 0, 1, 6, 0, 2)), 2..=3);
    marks_none(run(&mut r, |r| r.copy_rect(0, 0, 1, 6, 0, 500)));
}

#[test]
fn a_gl_copy_marks_the_zoomed_destination_rows() {
    let mut r = bare();
    column(&mut r, 0..8);
    // zx 1, zy 2: three source rows are six destination rows.
    marks(run(&mut r, |r| r.copy_rect_gl(0, 0, 1, 3, 10, 20, 1 | 2 << 8)), 20..=25);
    // Past the top only the rows on the frame buffer.
    let top = FB_H as i32 - 3;
    marks(run(&mut r, |r| r.copy_rect_gl(0, 0, 1, 3, 10, top, 1 | 2 << 8)), top as usize..=FB_H - 1);
}

#[test]
fn a_z_fill_marks_nothing() {
    let mut r = bare();
    marks_none(run(&mut r, |r| r.zfill(0x0010_0010_0000_0000 | 0x0020_0000_0000, 0x00ff_ffff_0000_0000 | 0x1234)));
}

// ---- the engine: what the shell takes and composes ----

fn rig() -> Rig {
    let mut rig = Rig::new(Gr2Variant::Xz);
    rig.tp_init();
    rig.program_display();
    rig.idle();
    rig
}

/// The picture a shell holds, kept up to date from the damage alone.
struct Viewer {
    w: usize,
    h: usize,
    shown: Vec<u32>,
}

impl Viewer {
    fn new(rig: &mut Rig) -> Viewer {
        let mut whole = Vec::new();
        let (w, h) = rig.e.scanout(0, &mut whole).unwrap().unwrap();
        let mut v = Viewer { w: w as usize, h: h as usize, shown: vec![0; whole.len()] };
        v.look(rig, "the first look");
        v
    }

    /// Take the damage, compose its rows into what is shown and compare with a whole compose.
    fn look(&mut self, rig: &mut Rig, what: &str) -> Vec<(u32, u32)> {
        rig.idle();
        let runs = rig.e.take_damage();
        for &(a, b) in &runs {
            assert_eq!(rig.e.scanout_rows(a, b, &mut self.shown).unwrap(), (self.w as u32, self.h as u32));
        }
        let mut whole = Vec::new();
        rig.e.scanout(0, &mut whole).unwrap();
        if let Some(y) = (0..self.h).find(|&y| self.shown[y * self.w..][..self.w] != whole[y * self.w..][..self.w]) {
            panic!("{what}: row {y} is stale after composing {runs:?}");
        }
        runs
    }
}

/// VRAM rows `lo` to `hi` as the display run they are.
fn display(lo: u32, hi: u32) -> (u32, u32) {
    (FB_H as u32 - 1 - hi, FB_H as u32 - lo)
}

#[test]
fn the_first_look_names_the_whole_picture_and_the_next_nothing() {
    let mut rig = Rig::new(Gr2Variant::Xz);
    assert_eq!(rig.e.take_damage(), vec![(0, 1024)]);
    assert_eq!(rig.e.take_damage(), vec![]);
}

#[test]
fn the_damage_stops_at_the_pictures_height() {
    // Monitor code 6, which the board shows as 1024 by 768.
    let mut rig = Rig::with_probe(Gr2Probe { ges: 4, ports: [0xdd, 0x04, 0x37, 0x15], hq_rev: 0, aux_window: AuxWindow::None });
    assert_eq!(rig.e.take_damage(), vec![(0, 768)]);
}

#[test]
fn a_rect_names_the_rows_it_filled() {
    let mut rig = rig();
    let mut v = Viewer::new(&mut rig);
    rig.rect(5, 100, 120, 300, 130);
    assert_eq!(v.look(&mut rig, "a rect"), vec![display(120, 130)]);
    // Two far apart are two runs.
    rig.rect(6, 0, 10, 50, 12);
    rig.rect(7, 0, 900, 50, 902);
    assert_eq!(v.look(&mut rig, "two rects"), vec![display(900, 902), display(10, 12)]);
    assert_eq!(v.look(&mut rig, "nothing drew"), vec![]);
}

#[test]
fn a_text_copy_names_the_rows_it_wrote() {
    let mut rig = rig();
    rig.rect(9, 0, 400, 40, 410);
    let mut v = Viewer::new(&mut rig);
    // A 10x10 block, X-style top row 20 (rows 994 to 1003), up by 16 pixels.
    rig.cmd(hq2::PUC_RECTCOPY, 3);
    for d in [4864 / 3, 0, 20, 10, 10, 0, 4] {
        rig.data(d);
    }
    // The destination: X rows 4 to 13, which are VRAM rows 1010 to 1019.
    assert_eq!(v.look(&mut rig, "a copy"), vec![display(1010, 1019)]);
}

#[test]
fn pixel_dma_names_the_rows_it_wrote() {
    let mut rig = rig();
    let mut v = Viewer::new(&mut rig);
    rig.cmd(hq2::X2D_SESSION_START, 0);
    rig.cmd(hq2::X2D_PLANE_SELECT, 0x1009);
    rig.cmd(hq2::X2D_FG_ALU, 0);
    for x in [0xff, 3, 0] {
        rig.data(x);
    }
    let (rows, wpr) = (40u32, 32u32);
    rig.w32(0x6a04c, 0);
    rig.cmd(hq2::PIXDMA_TO_SCREEN, 0);
    for x in [100, wpr * 4, rows, wpr, 0, 0] {
        rig.w32(0x40000, x);
    }
    let words: Vec<u32> = (0..rows * wpr).map(|i| i.wrapping_mul(0x0101_0101) | 0x0f0f_0f0f).collect();
    let mut at = 0;
    while at < words.len() {
        let n = rig.e.push_words(0x6a068, &words[at..]);
        at += n;
        if n == 0 {
            sgi_engine::shell::drain_stalls(&mut rig.e, &mut rig.now, 1_000_000, 1);
        }
    }
    let runs = v.look(&mut rig, "pixel DMA");
    assert!(runs.len() == 1 && runs[0].1 - runs[0].0 == rows, "{runs:?}");
}

// ---- what changes the whole picture, and what only the cursor ----

const XMAP: u32 = 0x6c1a0;
const VC1: u32 = 0x6c040;

fn vc1_reg(rig: &mut Rig, reg: u32, val: u32) {
    rig.w32(VC1 + 0x10, reg);
    rig.w32(VC1 + 0x14, 0);
    rig.w32(VC1, val);
}

fn vc1_sram(rig: &mut Rig, at: u32, val: u32) {
    rig.w32(VC1 + 0x10, at & 0xff);
    rig.w32(VC1 + 0x14, at >> 8);
    rig.w32(VC1 + 8, val);
}

#[test]
fn a_change_to_a_table_the_compose_reads_names_every_row() {
    for (what, step) in [
        ("a colour map entry", Box::new(|r: &mut Rig| {
            r.w32(XMAP + 0x14, 0x10);
            r.w32(XMAP + 0x10, 7);
            r.w8(XMAP + 8, 0x90);
        }) as Box<dyn Fn(&mut Rig)>),
        ("a mode word", Box::new(|r| r.w32(XMAP + 0x04, 0x8300_0000))),
        ("a ramp DAC entry", Box::new(|r| r.w32(0x6c0a0 + 4, 9))),
        ("a window ID table", Box::new(|r| vc1_reg(r, 0x40, 0x0b00))),
        ("a window ID line table", Box::new(|r| vc1_sram(r, 0x3000, 0x7ff4))),
    ] {
        let mut rig = rig();
        let mut v = Viewer::new(&mut rig);
        step(&mut rig);
        assert_eq!(v.look(&mut rig, what), vec![(0, 1024)], "{what}");
    }
}

/// A colour map store after its address is set, the look in between taking the address stores' damage.
#[test]
fn a_colour_map_data_store_names_every_row() {
    // The all-heads window, then one head's registers; 8-bit stores take the narrow path.
    for (what, base) in [("all heads", XMAP), ("one head", 0x6c100)] {
        let mut rig = rig();
        let mut v = Viewer::new(&mut rig);
        rig.w32(base + 0x14, 0x10);
        rig.w32(base + 0x10, 0);
        v.look(&mut rig, "the address");
        for c in [0x90, 0x40, 0x10] {
            rig.w8(base + 8, c);
        }
        assert_eq!(v.look(&mut rig, what), vec![(0, 1024)], "{what}, 8-bit");
        rig.w32(base + 0x10, 0);
        v.look(&mut rig, "the address again");
        rig.w32(base + 8, 0x0001_0203);
        assert_eq!(v.look(&mut rig, what), vec![(0, 1024)], "{what}, 32-bit");
    }
}

#[test]
fn a_vc1_address_write_names_nothing() {
    let mut rig = rig();
    let mut v = Viewer::new(&mut rig);
    rig.w32(VC1 + 0x10, 0x40);
    rig.w32(VC1 + 0x14, 0x12);
    rig.w32(VC1 + 4, 1);
    assert_eq!(v.look(&mut rig, "addresses"), vec![]);
}

/// A cursor glyph whose every pixel is colour 1, at the glyph address 0x1000, with colour 1 white, the
/// cursor at (x, y) and still off.
fn load_cursor(rig: &mut Rig, x: u32, y: u32) {
    for w in 0..64 {
        vc1_sram(rig, 0x1000 + 2 * w, 0xffff);
    }
    vc1_reg(rig, 0x20, 0x1000);
    vc1_reg(rig, 0x22, x);
    vc1_reg(rig, 0x24, y + 35);
    rig.w32(XMAP + 0x14, 0x1c);
    rig.w32(XMAP + 0x10, 1);
    for _ in 0..3 {
        rig.w8(XMAP + 8, 0xff);
    }
}

const CURSOR_ON: u32 = 0x30;

#[test]
fn the_cursor_names_its_old_and_new_rows() {
    let mut rig = rig();
    load_cursor(&mut rig, 200, 300);
    let mut v = Viewer::new(&mut rig);
    rig.w32(VC1 + 0x18, CURSOR_ON);
    assert_eq!(v.look(&mut rig, "the cursor appears"), vec![(300, 332)]);
    // A move down 100 rows: the rows it left and the rows it reached.
    vc1_reg(&mut rig, 0x24, 400 + 35);
    assert_eq!(v.look(&mut rig, "the cursor moves"), vec![(300, 332), (400, 432)]);
    // A move along a row changes the same rows.
    vc1_reg(&mut rig, 0x22, 500);
    assert_eq!(v.look(&mut rig, "the cursor moves along"), vec![(400, 432)]);
    // A new glyph and the cursor going away.
    vc1_sram(&mut rig, 0x1000, 0);
    assert_eq!(v.look(&mut rig, "the glyph changes"), vec![(400, 432)]);
    rig.w32(VC1 + 0x18, 0);
    assert_eq!(v.look(&mut rig, "the cursor hides"), vec![(400, 432)]);
}

#[test]
fn a_table_stored_before_any_cursor_address_names_every_row() {
    let mut rig = rig();
    let mut v = Viewer::new(&mut rig);
    // The glyph address is still 0, and SRAM 0 to 0x100 is where a table may go.
    vc1_sram(&mut rig, 0x40, 0x1234);
    assert_eq!(v.look(&mut rig, "a table in low SRAM"), vec![(0, 1024)]);
}

#[test]
fn the_cursor_at_the_edge_names_the_rows_on_the_picture() {
    let mut rig = rig();
    load_cursor(&mut rig, 10, 1010);
    let mut v = Viewer::new(&mut rig);
    rig.w32(VC1 + 0x18, CURSOR_ON);
    assert_eq!(v.look(&mut rig, "the cursor at the bottom"), vec![(1010, 1024)]);
    vc1_reg(&mut rig, 0x24, 35u32.wrapping_sub(10));
    assert_eq!(v.look(&mut rig, "the cursor above the top"), vec![(0, 22), (1010, 1024)]);
}

// ---- state that replaces the board ----

#[test]
fn a_loaded_state_and_a_reset_name_every_row() {
    let mut a = rig();
    a.rect(5, 0, 10, 100, 20);
    let mut now = 0;
    sgi_engine::shell::quiesce(&mut a.e, &mut now, 1_000_000, 100).unwrap();
    let mut chunks: Vec<Chunk> = Vec::new();
    a.e.save(&mut chunks).unwrap();
    let mut b = rig();
    let mut v = Viewer::new(&mut b);
    sgi_engine::shell::quiesce(&mut b.e, &mut now, 1_000_000, 100).unwrap();
    b.e.load(&mut VecSource::new(chunks)).unwrap();
    assert_eq!(v.look(&mut b, "after the load"), vec![(0, 1024)], "a load");
    b.e.reset().unwrap();
    assert_eq!(v.look(&mut b, "after the reset"), vec![(0, 1024)], "a reset");
}

#[test]
fn a_board_told_its_picture_may_differ_names_every_row() {
    let mut rig = rig();
    Viewer::new(&mut rig);
    rig.e.board().mark_dirty();
    assert_eq!(rig.e.take_damage(), vec![(0, 1024)]);
}

#[test]
fn rows_are_composed_into_their_place_and_nothing_else() {
    let mut rig = rig();
    rig.rect(5, 0, 10, 100, 20);
    rig.idle();
    let mut whole = Vec::new();
    rig.e.scanout(0, &mut whole).unwrap();
    let mut out = vec![0xdead_beef; 1280 * 1024];
    assert_eq!(rig.e.scanout_rows(1003, 1015, &mut out).unwrap(), (1280, 1024));
    for y in 0..1024 {
        let row = &out[y * 1280..][..1280];
        if (1003..1015).contains(&y) {
            assert_eq!(row, &whole[y * 1280..][..1280], "row {y}");
        } else {
            assert!(row.iter().all(|&p| p == 0xdead_beef), "row {y} is untouched");
        }
    }
    // A buffer too short for the picture is left alone.
    let mut short = vec![0xdead_beef; 1280];
    assert_eq!(rig.e.scanout_rows(0, 1, &mut short).unwrap(), (1280, 1024));
    assert!(short.iter().all(|&p| p == 0xdead_beef));
}

// ---- GL operations ----

/// The window box and view state of `gl_setup_window` in the board tests: 400 by 300 at (64, 660).
fn gl_window(rig: &mut Rig) {
    let fl = |v: f32| v.to_bits();
    rig.cmd(0x1e5, 64);
    for v in [660, 400, 300, 0x10, 0, 1, (463 << 11) | 64, (959 << 10) | 660, 0, 0, 0, 0, 0, 0] {
        rig.data(v);
    }
    rig.cmd(0x03b, fl(1.0));
    for v in [0.0f32, 399.0, 0.0, 299.0, 1073741823.0, 1073741823.0] {
        rig.data(fl(v));
    }
    rig.cmd(0x013, 0);
}

#[test]
fn a_gl_copy_names_the_rows_it_wrote() {
    let mut rig = rig();
    // A source row at VRAM row 680 (window row 20), copied to window row 40, which is VRAM row 700.
    rig.rect(5, 74, 680, 76, 680);
    let mut v = Viewer::new(&mut rig);
    gl_window(&mut rig);
    rig.cmd(0x0ba, 0);
    for d in [20, 40, 1, 10, 50, 3] {
        rig.data(d);
    }
    assert_eq!(v.look(&mut rig, "a GL copy"), vec![display(700, 700)]);
}

#[test]
fn a_z_clear_names_nothing() {
    let mut rig = rig();
    let mut v = Viewer::new(&mut rig);
    gl_window(&mut rig);
    // A depth-only clear: colour in the command, then depth and the colour planes (none).
    rig.cmd(0x68a0, 0);
    for d in [0x1234, 0] {
        rig.data(d);
    }
    assert_eq!(v.look(&mut rig, "a Z clear"), vec![]);
}
