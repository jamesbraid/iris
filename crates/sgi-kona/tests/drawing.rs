//! What the QEMU KONA qtests and iris's own suite check about drawing, as values: X fills, pixel
//! writes, copies and reads, stipples, GL points, lines, textures and pixel operations, and image DMA
//! into the planes. They drive the back end through `Rig`, with no board, thread or machine, and
//! are ported from `back/tests.rs` of iris's KONA files, which stay with iris.

use sgi_kona::dev::kona::defs::gl::{BACK, COLOR_BUFFER_BIT, LINE_STIPPLE, POLYGON_STIPPLE, PROJECTION, TEXTURE_2D};
use sgi_kona::dev::kona::defs::pixel::*;
use sgi_kona::dev::kona::defs::*;
use sgi_kona::dev::kona::ident::KonaConfig;
use sgi_kona::dev::kona::job::{ContextEvent, Grammar, Job, Reply};
use sgi_kona::dev::kona::rig::{Rig, REASON_PAGE, RETURN_PAGE};

fn rig() -> Rig {
    Rig::new(KonaConfig::default())
}

/// The X graphics context the box and pixel tests draw under: a 16-bit index context, foreground
/// 0x5a00 in colour word 1.
const GC: [u32; 13] = [0x5a000, 0x1ffe, 0xa5000, 0x1ffe, 0xf00fff, 0, 0x180c, 0x40001, 1, 6, 5, 1, 1];

fn lit(r: &mut Rig, x: u32, y: u32) -> u32 {
    r.read(x, y, 1)
}

/// An X box draws into colour word 1 of the store under the loaded context, and a command that is
/// no X token leaves a waiting run alone only if it belongs to the run.
#[test]
fn an_x_box_draws_into_the_store() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(FILL_BOXES_TWO_CORNERS, &[2 << 16 | 3, 5 << 16 | 7]);
    assert_eq!(lit(&mut r, 2, 1023 - 3), 0x5a00);
    assert_eq!(lit(&mut r, 5, 1023 - 3), 0, "the far edge is out");
    // A data token, another command, then a box: the run did not survive.
    r.x(WINDOW_SYSTEM_2_WORD_DATA_PRELOAD, &[10 << 16 | 10, 12 << 16 | 12]);
    r.x(PIPE_SET_UP, &[0, 0, 0]);
    r.x(FILL_BOXES_TWO_CORNERS, &[20 << 16 | 20, 21 << 16 | 21]);
    assert_eq!(lit(&mut r, 10, 1023 - 10), 0);
    assert_eq!(lit(&mut r, 20, 1023 - 20), 0x5a00);
}

/// A stipple store asks for its rows as a data run; with the pattern stored and loaded, a box fill
/// is masked by it, and with none loaded it is solid.
#[test]
fn a_stipple_is_stored_loaded_and_masks_a_fill() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    // A 2 x 2 checkerboard in entry 4: the reply asks for its two rows.
    assert_eq!(r.x(STORE_WINDOW_SYSTEM_STIPPLE, &[4, 2, 2]), Reply::Data(2));
    r.send(Job::Data { grammar: Grammar::Kernel, selector: STORE_WINDOW_SYSTEM_STIPPLE, args: &[4, 2, 2], words: &[0x8000_0000, 0x4000_0000] });
    assert_eq!(r.x(LOAD_WINDOW_SYSTEM_STIPPLE, &[4]), Reply::Done);
    r.x(FILL_BOXES_TWO_CORNERS, &[0, 4 << 16 | 4]);
    assert_eq!(lit(&mut r, 0, 1023), 0x5a00);
    assert_eq!(lit(&mut r, 1, 1023), 0);
    assert_eq!(lit(&mut r, 1, 1022), 0x5a00);
    r.x(LOAD_WINDOW_SYSTEM_STIPPLE, &[u32::MAX]);
    r.x(FILL_BOXES_TWO_CORNERS, &[10 << 16 | 10, 12 << 16 | 12]);
    assert_eq!(lit(&mut r, 11, 1023 - 10), 0x5a00, "no stipple: solid");
    assert_eq!(r.x(STORE_WINDOW_SYSTEM_STIPPLE, &[4, 2, 0]), Reply::Done, "a store of no rows asks for none");
}

/// An image whose rows follow the command as a data run draws once the run is in, the store's top
/// row first.
#[test]
fn an_inline_pixel_write_asks_for_its_rows_and_draws_them() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0, 0, 1, 8, 3, 2, 0]);
    assert_eq!(r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[4, 2]), Reply::Data(2));
    r.send(Job::Data { grammar: Grammar::Kernel, selector: WINDOW_SYSTEM_PIXEL_WRITE, args: &[4, 2], words: &[0x0102_0300, 0x0405_0600] });
    assert_eq!([r.read(4, 1021, 1), r.read(6, 1021, 1), r.read(5, 1020, 1)], [0x100, 0x300, 0x500]);
    assert_eq!(r.x(WINDOW_SYSTEM_PIXEL_PATH_FINISH, &[0]), Reply::Done);
}

/// With the DMA bit set the command asks for nothing and the DMA packet that arrives next carries
/// the rows, read from the guest a line at a time; one that arrives with no write waiting is
/// unclaimed, and a read that fails faults the packet and draws nothing.
#[test]
fn a_dma_pixel_write_takes_the_packet_that_follows() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0x10, 0, 1, 8, 4, 2, 0]);
    assert_eq!(r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[0, 0]), Reply::Done);
    // Two rows of one word each, the lines 8 bytes apart.
    r.put_words(0x40_0000, &[0x0102_0304, 0xdead_beef, 0x0506_0708]);
    let dma = |io, lines, per_line, pitch| Job::DmaIn { io, physical: true, lines, per_line, pitch };
    assert_eq!(r.send(dma(0x40_0000, 2, 4, 8)), Reply::Done);
    assert_eq!([r.read(0, 1023, 1), r.read(3, 1023, 1), r.read(3, 1022, 1)], [0x100, 0x400, 0x800]);
    assert_eq!(r.send(dma(0x40_0000, 1, 4, 4)), Reply::Unclaimed, "nothing waits for it");
    assert!(!r.dma_fault());
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[0x10, 0, 1, 8, 4, 1, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_WRITE, &[0, 4]);
    assert_eq!(r.send(dma(0x7f_ff_ff_00, 1, 4, 4)), Reply::Done);
    assert!(r.dma_fault(), "the bus read failed");
    assert_eq!(r.read(0, 1019, 1), 0);
}

/// A read answers on the return page after its first word, which is set last; with no context
/// current it is not answered.
#[test]
fn a_pixel_read_answers_on_the_return_page() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(FILL_BOXES_TWO_CORNERS, &[2 << 16, 4 << 16 | 1]);
    assert_eq!(r.read(3, 1023, 1), 0x5a00);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[2, 1, 0, 8, 3, 1, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_READ, &[2, 0]);
    assert_eq!(r.word(RETURN_PAGE), 1);
    assert_eq!(r.bytes(RETURN_PAGE + 8, 4), [0x5a, 0x5a, 0, 0], "pixels 2 and 3 are filled, 4 is not");
    r.put(RETURN_PAGE, &[0; 16]);
    r.set_current(None);
    r.x(WINDOW_SYSTEM_PIXEL_READ, &[2, 0]);
    assert_eq!(r.word(RETURN_PAGE), 0, "no context, no answer");
}

/// A copy under the context moves pixels the same way a write puts them.
#[test]
fn a_pixel_copy_moves_pixels_inside_the_store() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.x(FILL_BOXES_TWO_CORNERS, &[0, 2 << 16 | 1]);
    r.x(WINDOW_SYSTEM_PIXEL_PATH_SET_UP, &[1, 1, 1, 0, 2, 1, 0]);
    r.x(WINDOW_SYSTEM_PIXEL_COPY, &[0, 0, 10, 5]);
    assert_eq!([r.read(10, 1018, 1), r.read(11, 1018, 1), r.read(12, 1018, 1)], [0x5a00, 0x5a00, 0]);
}

/// Each context has its own X state, and a fresh activation starts over.
#[test]
fn the_x_state_follows_the_context() {
    let mut r = rig();
    r.x(WINDOW_SYSTEM_GRAPHICS_CONTEXT_LOAD, &GC);
    r.set_current(Some(1));
    r.send(Job::Context(ContextEvent::Activated { slot: 1, fresh: true }));
    r.x(FILL_BOXES_TWO_CORNERS, &[0, 2 << 16 | 2]);
    assert_eq!(r.read(0, 1023, 1), 0, "context 1 loaded no colour");
    r.set_current(Some(0));
    r.x(FILL_BOXES_TWO_CORNERS, &[0, 2 << 16 | 2]);
    assert_eq!(r.read(1, 1022, 1), 0x5a00, "context 0 kept its own");
}

/// A 64 x 64 GL window at the origin, double buffered, with a viewport that maps clip coordinates -1
/// to 1 onto it; draws go to colour word 2.
fn gl_window() -> Rig {
    let mut r = rig();
    r.x(WINDOW_CLIP_SETUP, &[0, 0, 64, 64, 0, 0, 64, 64, 0, 13, 0]);
    r.command(Grammar::Gl, CONFIGURE_FRAME_BUFFER_LAYOUT, &[0, 8, 0, 1, 0, 0, 24, 0, 0, 0]);
    r.command(Grammar::Gl, SET_VIEWPORT, &[0, 0, 64, 64]);
    r.command(Grammar::Gl, SET_CURRENT_COLOUR_4_COMPONENTS_SINGLE_FLOAT, &[1.0f32.to_bits(); 4]);
    r
}

/// Window coordinate to clip coordinate for `gl_window`'s 64-pixel viewport.
fn ndc(w: f32) -> u32 {
    (w / 32.0 - 1.0).to_bits()
}

fn vertices(r: &mut Rig, mode: u32, points: &[(f32, f32)]) {
    r.command(Grammar::Gl, mode, &[0]);
    for &(x, y) in points {
        r.command(Grammar::Gl, SUBMIT_VERTEX_3_COMPONENTS_SINGLE_FLOAT, &[ndc(x), ndc(y), 0.0f32.to_bits()]);
    }
    r.command(Grammar::Gl, END_OPEN_PRIMITIVE, &[0]);
}

fn lit_pixels(r: &mut Rig) -> Vec<(u32, u32)> {
    (0..64).flat_map(|y| (0..64u32).map(move |x| (x, y))).filter(|&(x, y)| r.read(x, y, 2) != 0).collect()
}

/// A point lights the pixel it lands in, or a disc when it is wide.
#[test]
fn a_point_lights_one_pixel_or_a_disc() {
    for (x, y, want) in [(10.5, 10.5, (10, 10)), (20.0, 30.0, (20, 30)), (7.9, 3.0, (7, 3))] {
        let mut r = gl_window();
        vertices(&mut r, BEGIN_PRIMITIVE_POINTS, &[(x, y)]);
        assert_eq!(lit_pixels(&mut r), [want], "a point at ({x}, {y})");
    }
    let mut r = gl_window();
    r.command(Grammar::Gl, SET_POINT_SIZE, &[3.0f32.to_bits(), 0]);
    vertices(&mut r, BEGIN_PRIMITIVE_POINTS, &[(10.5, 10.5)]);
    assert_eq!(lit_pixels(&mut r), [(10, 10)], "size 3 is still one pixel");
    let mut r = gl_window();
    r.command(Grammar::Gl, SET_POINT_SIZE, &[8.0f32.to_bits(), 0]);
    vertices(&mut r, BEGIN_PRIMITIVE_POINTS, &[(32.0, 32.0)]);
    let lit = lit_pixels(&mut r);
    assert!((44..=56).contains(&lit.len()), "a disc of diameter 8 holds about 50 pixels: {}", lit.len());
    assert!(lit.iter().all(|&(x, y)| (27..=36).contains(&x) && (27..=36).contains(&y)));
}

/// A line of width 1 lights one row, from just past its first end to its last pixel (the fill rule
/// leaves the left edge out); a wide line lights its width.
#[test]
fn a_line_lights_its_width() {
    let mut r = gl_window();
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_LINES, &[(10.5, 20.5), (40.5, 20.5)]);
    let lit = lit_pixels(&mut r);
    assert!(lit.iter().all(|&(_, y)| y == 20), "one row");
    assert_eq!((lit.first().map(|p| p.0), lit.last().map(|p| p.0), lit.len()), (Some(11), Some(40), 30));
    let mut r = gl_window();
    r.command(Grammar::Gl, SET_LINE_WIDTH, &[3.0f32.to_bits(), 0]);
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_LINES, &[(10.5, 20.5), (40.5, 20.5)]);
    let rows: std::collections::BTreeSet<u32> = lit_pixels(&mut r).iter().map(|p| p.1).collect();
    assert_eq!(rows, [19, 20, 21].into(), "width 3");
}

/// Line stipples keep the pixels their bits name, a repeat stretches each bit, and a polygon stipple
/// is a checkerboard of the pattern loaded.
#[test]
fn stipples_keep_the_pixels_their_patterns_name() {
    let mut r = gl_window();
    r.command(Grammar::Gl, ENABLE_CAPABILITY, &[LINE_STIPPLE]);
    r.command(Grammar::Gl, SET_LINE_STIPPLE, &[1, 0x00ff]);
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_LINES, &[(10.5, 20.5), (40.5, 20.5)]);
    let xs: Vec<u32> = lit_pixels(&mut r).iter().map(|p| p.0).collect();
    let want: Vec<u32> = (11..=40).filter(|x| (x - 10) % 16 < 8).collect();
    assert_eq!(xs, want, "the low eight bits of the pattern: eight pixels on, eight off");
    let mut r = gl_window();
    r.command(Grammar::Gl, ENABLE_CAPABILITY, &[LINE_STIPPLE]);
    r.command(Grammar::Gl, SET_LINE_STIPPLE, &[2, 0x00ff]);
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_LINES, &[(10.5, 20.5), (50.5, 20.5)]);
    let xs: Vec<u32> = lit_pixels(&mut r).iter().map(|p| p.0).collect();
    let want: Vec<u32> = (11..=50).filter(|x| (x - 10) / 2 % 16 < 8).collect();
    assert_eq!(xs, want);
    let mut r = gl_window();
    let mut rows = [0xaaaa_aaaau32; 32];
    rows.iter_mut().skip(1).step_by(2).for_each(|r| *r = 0x5555_5555);
    assert_eq!(r.command(Grammar::Gl, LOAD_POLYGON_STIPPLE_PATTERN, &[0, 0, 0]), Reply::Data(32));
    r.send(Job::Data { grammar: Grammar::Gl, selector: LOAD_POLYGON_STIPPLE_PATTERN, args: &[0, 0, 0], words: &rows });
    r.command(Grammar::Gl, ENABLE_CAPABILITY, &[POLYGON_STIPPLE]);
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_TRIANGLES, &[(0.5, 0.5), (60.5, 0.5), (0.5, 60.5)]);
    let lit = lit_pixels(&mut r);
    assert!(lit.len() > 500 && lit.iter().all(|&(x, y)| (x + y) % 2 == 0), "a checkerboard of {} pixels", lit.len());
    let mut r = gl_window();
    r.send(Job::Data { grammar: Grammar::Gl, selector: LOAD_POLYGON_STIPPLE_PATTERN, args: &[0, 0, 0], words: &rows });
    vertices(&mut r, BEGIN_PRIMITIVE_INDEPENDENT_TRIANGLES, &[(0.5, 0.5), (60.5, 0.5), (0.5, 60.5)]);
    assert!(lit_pixels(&mut r).iter().any(|&(x, y)| (x + y) % 2 == 1), "with the stipple off the triangle is solid");
}

/// A texture loaded inline, bound by its short bind and record, and drawn with the replace
/// environment, shows its texels at nearest and blends them at linear; with the texture off the
/// draw is the vertex colour.
#[test]
fn a_loaded_and_bound_texture_shows_on_a_triangle() {
    let mut r = gl_window();
    let mut hdr = [0u32; 20];
    hdr[..4].copy_from_slice(&[0x0de1, 2, 2, 5]);
    hdr[4] = 1;
    hdr[9] = 0x30;
    hdr[13] = 1;
    hdr[14] = 1;
    hdr[16] = 0x11;
    hdr[17] = 0x2200;
    hdr[18] = 0x1000;
    let image = [0xff00_00ffu32, 0x00ff_00ff, 0x0000_ffff, 0xffff_ffff];
    assert_eq!(r.command(Grammar::Gl, DRAW_TEXELS_INTO_TEXTURE_MEMORY, &hdr), Reply::Data(4));
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_TEXELS_INTO_TEXTURE_MEMORY, args: &hdr, words: &image });
    r.command(Grammar::Gl, TEXTURE_LOAD_BY_IDENTIFIER, &[0]);
    let mut rec = [0u32; 22];
    rec[0] = 0x44;
    rec[1] = 0x1010;
    rec[6] = 0x1000;
    rec[7] = 0xffff_ffff;
    let bind = |r: &mut Rig, mag: u32| {
        r.command(Grammar::Gl, BIND_TEXTURE_CURRENT_FORM, &[0xffff_ffff, 2 << 12, mag << 5, 0x0011, 1]);
        r.command(Grammar::Gl, BIND_2D_MIP_MAPPED_TEXTURE, &rec);
    };
    bind(&mut r, 0);
    r.command(Grammar::Gl, SET_TEXTURE_ENVIRONMENT_PARAMETER_FLOAT_FORM, &[0x2200, (0x1e01 as f32).to_bits(), 0, 0, 0]);
    r.command(Grammar::Gl, ENABLE_CAPABILITY, &[TEXTURE_2D]);
    let quad = |r: &mut Rig| {
        r.command(Grammar::Gl, BEGIN_PRIMITIVE_INDEPENDENT_TRIANGLES, &[0]);
        for (x, y, s, t) in [(0.0, 0.0, 0.0, 0.0), (64.0, 0.0, 1.0, 0.0), (0.0, 64.0, 0.0, 1.0), (64.0, 0.0, 1.0, 0.0), (64.0, 64.0, 1.0, 1.0), (0.0, 64.0, 0.0, 1.0)] {
            r.command(Grammar::Gl, SET_CURRENT_TEXTURE_COORDINATE_2_COMPONENTS_SINGLE_FLOAT, &[(s as f32).to_bits(), (t as f32).to_bits()]);
            r.command(Grammar::Gl, SUBMIT_VERTEX_3_COMPONENTS_SINGLE_FLOAT, &[ndc(x), ndc(y), 0.0f32.to_bits()]);
        }
        r.command(Grammar::Gl, END_OPEN_PRIMITIVE, &[0]);
    };
    quad(&mut r);
    assert_eq!([r.read(10, 10, 2), r.read(50, 10, 2), r.read(10, 50, 2), r.read(50, 50, 2)], [0xffff_0000, 0xff00_ff00, 0xff00_00ff, 0xffff_ffff], "nearest: each quadrant its texel");
    bind(&mut r, 1);
    quad(&mut r);
    let mid = r.read(32, 32, 2);
    assert!([mid >> 16 & 0xff, mid >> 8 & 0xff, mid & 0xff].iter().all(|&c| (100..160).contains(&c)), "{mid:#x}: red, green, blue and white average to grey");
    r.command(Grammar::Gl, DISABLE_CAPABILITY, &[TEXTURE_2D]);
    quad(&mut r);
    assert_eq!(r.read(10, 10, 2), 0xffff_ffff);
}

/// `gl_window` with a window-sized orthographic projection, so a raster position is a window pixel.
fn pixel_window() -> Rig {
    let mut r = gl_window();
    r.command(Grammar::Gl, SELECT_CURRENT_MATRIX_STACK, &[PROJECTION]);
    r.command(Grammar::Gl, LOAD_MATRIX_16_FLOATS, &ortho64().map(f32::to_bits));
    r
}

/// The orthographic matrix of a 64 by 64 window, column-major, as the client's matrix is.
fn ortho64() -> [f32; 16] {
    let mut m = [0.0f32; 16];
    (m[0], m[5], m[10], m[15]) = (2.0 / 64.0, 2.0 / 64.0, -1.0, 1.0);
    (m[12], m[13]) = (-1.0, -1.0);
    m
}

fn at(r: &mut Rig, x: i32, y: i32) {
    r.command(Grammar::Gl, SET_RASTER_POSITION_2_COMPONENTS_32_BIT_INTEGER, &[x as u32, y as u32]);
}

/// A pixel draw: the header, then the image words the back end asked for.
fn draw_pixels(r: &mut Rig, w: u32, h: u32, format: u32, ty: u32, image: &[u32]) {
    let hdr = [w, h, format, ty, 0, 0, 0, 0];
    assert_eq!(r.command(Grammar::Gl, DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, &hdr), if image.is_empty() { Reply::Done } else { Reply::Data(image.len() as u32) });
    if !image.is_empty() {
        r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, args: &hdr, words: image });
    }
}

fn colour_at(r: &mut Rig, x: u32, y: u32) -> u32 {
    r.read(x, y, 2)
}

/// A 2 x 2 RGBA image of bytes lands with its lower-left pixel at the raster position; zoom
/// replicates each pixel; a raster position outside the clip volume draws nothing.
#[test]
fn draw_pixels_lands_at_the_raster_position_and_zooms() {
    let mut r = pixel_window();
    at(&mut r, 10, 20);
    draw_pixels(&mut r, 2, 2, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0xff00_00ff, 0x00ff_00ff, 0x0000_ffff, 0xffff_ffff]);
    assert_eq!([colour_at(&mut r, 10, 20), colour_at(&mut r, 11, 20), colour_at(&mut r, 10, 21), colour_at(&mut r, 11, 21)], [0xffff_0000, 0xff00_ff00, 0xff00_00ff, 0xffff_ffff]);
    assert_eq!(colour_at(&mut r, 12, 20), 0, "nothing past the image");
    r.command(Grammar::Gl, SET_PIXEL_ZOOM_FACTORS, &[2.0f32.to_bits(), 3.0f32.to_bits()]);
    at(&mut r, 30, 30);
    draw_pixels(&mut r, 1, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0x1020_30ff]);
    let lit = (28..40u32).flat_map(|y| (28..40u32).map(move |x| (x, y))).filter(|&(x, y)| colour_at(&mut r, x, y) == 0xff10_2030).collect::<Vec<_>>();
    assert_eq!(lit, vec![(30, 30), (31, 30), (30, 31), (31, 31), (30, 32), (31, 32)], "two wide and three high");
    at(&mut r, 100, 100);
    draw_pixels(&mut r, 1, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0xffff_ffff]);
    assert_eq!(colour_at(&mut r, 63, 63), 0);
}

/// The pixel-store tokens set the unpack record that sizes the next draw; the transfer scale and
/// bias, and a pixel map under its flag, change what is drawn.
#[test]
fn unpack_state_transfer_and_maps_shape_a_draw() {
    let mut r = pixel_window();
    at(&mut r, 5, 5);
    // 3 x 1 RGB bytes: nine bytes are three words, with alignment 4 or 1.
    let hdr = [3, 1, FORMAT_RGB, TYPE_UNSIGNED_BYTE, 0, 0, 0, 0];
    assert_eq!(r.command(Grammar::Gl, DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, &hdr), Reply::Data(3));
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, args: &hdr, words: &[0, 0, 0] });
    // Rows of 3 x 3 bytes at alignment 4 pad to 12 bytes, so two rows are six words.
    let hdr2 = [3, 2, FORMAT_RGB, TYPE_UNSIGNED_BYTE, 0, 0, 0, 0];
    assert_eq!(r.command(Grammar::Gl, DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, &hdr2), Reply::Data(6));
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, args: &hdr2, words: &[0; 6] });
    r.command(Grammar::Gl, SET_PIXEL_STORE_PARAMETER_INTEGER_FORM, &[UNPACK_BASE + 5, 1]);
    assert_eq!(r.command(Grammar::Gl, DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, &hdr2), Reply::Data(5), "alignment 1 packs the rows");
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, args: &hdr2, words: &[0; 5] });
    // A red scale of 0.5.
    r.command(Grammar::Gl, SET_PIXEL_TRANSFER_PARAMETER_FLOAT_FORM, &[RED_SCALE, 0.5f32.to_bits()]);
    at(&mut r, 20, 20);
    draw_pixels(&mut r, 1, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0xff00_00ff]);
    let c = colour_at(&mut r, 20, 20);
    assert!((0x7f..=0x80).contains(&(c >> 16 & 0xff)), "{c:#x}");
    // A red-to-red map of two entries flips red once its flag is set.
    r.command(Grammar::Gl, SET_PIXEL_TRANSFER_PARAMETER_FLOAT_FORM, &[RED_SCALE, 1.0f32.to_bits()]);
    let hdr = [MAP_RED_TO_RED, 2, 1.0f32.to_bits(), 0];
    assert_eq!(r.command(Grammar::Gl, LOAD_PIXEL_MAP_FLOAT_VALUES, &hdr), Reply::Data(2));
    r.send(Job::Data { grammar: Grammar::Gl, selector: LOAD_PIXEL_MAP_FLOAT_VALUES, args: &hdr, words: &[1.0f32.to_bits(), 0.0f32.to_bits()] });
    r.command(Grammar::Gl, SET_PIXEL_TRANSFER_PARAMETER_FLOAT_FORM, &[MAP_COLOUR, 1]);
    at(&mut r, 22, 20);
    draw_pixels(&mut r, 1, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0xff00_00ff]);
    assert_eq!(colour_at(&mut r, 22, 20) >> 16 & 0xff, 0);
}

/// A bitmap lights its set bits in the raster colour, most significant bit leftmost, and moves the
/// position by its advance.
#[test]
fn a_small_bitmap_draws_and_advances() {
    let mut r = pixel_window();
    r.command(Grammar::Gl, SET_CURRENT_COLOUR_4_COMPONENTS_SINGLE_FLOAT, &[1.0f32.to_bits(), 0, 0, 1.0f32.to_bits()]);
    at(&mut r, 8, 8);
    // Two rows of eight: 1010 0000 and 0000 0001. Origin (-1, 0): the glyph sits one pixel right.
    let args = [8, 2, (-1.0f32).to_bits(), 0, 7.0f32.to_bits(), 0, 2, 2];
    assert_eq!(r.command(Grammar::Gl, DRAW_BITMAP_FROM_DISPLAY_LIST, &args), Reply::Data(1));
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_BITMAP_FROM_DISPLAY_LIST, args: &args, words: &[0xa001_0000] });
    assert_eq!(lit_pixels(&mut r), vec![(9, 8), (11, 8), (16, 9)]);
    // The advance moved the position to x = 16: the next bitmap starts there.
    let one = [8, 1, 0, 0, 8.0f32.to_bits(), 0, 2, 1];
    r.command(Grammar::Gl, DRAW_BITMAP_FROM_DISPLAY_LIST, &one);
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_BITMAP_FROM_DISPLAY_LIST, args: &one, words: &[0x8000_0000] });
    assert!(lit_pixels(&mut r).contains(&(16, 8)));
}

/// A copy takes the read buffer's rectangle to the raster position.
#[test]
fn a_gl_copy_takes_the_read_buffers_rectangle_to_the_raster_position() {
    let mut r = pixel_window();
    at(&mut r, 4, 4);
    draw_pixels(&mut r, 2, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0x1122_33ff, 0x4455_66ff]);
    r.command(Grammar::Gl, SELECT_READ_BUFFER, &[BACK]);
    r.command(Grammar::Gl, SELECT_DRAW_BUFFER, &[BACK]);
    at(&mut r, 30, 30);
    r.command(Grammar::Gl, COPY_PIXEL_RECTANGLE_WITHIN_FRAME_BUFFER, &[4, 4, 2, 1, 0, COPY_COLOUR, 0]);
    assert_eq!([colour_at(&mut r, 30, 30), colour_at(&mut r, 31, 30)], [0xff11_2233, 0xff44_5566]);
}

/// Accumulation: load the colour buffer scaled, add, multiply, and return to the draw buffer.
#[test]
fn accumulation_loads_scales_and_returns() {
    let mut r = pixel_window();
    r.command(Grammar::Gl, CONFIGURE_FRAME_BUFFER_LAYOUT, &[0, 8, 0, 1, 0, 0, 24, 0, 16, 0]);
    at(&mut r, 3, 3);
    draw_pixels(&mut r, 1, 1, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, &[0x8040_00ff]);
    r.command(Grammar::Gl, SELECT_READ_BUFFER, &[BACK]);
    let acc = |r: &mut Rig, op: u32, v: f32| {
        r.command(Grammar::Gl, ACCUMULATION_BUFFER_OPERATION, &[op, v.to_bits()]);
    };
    acc(&mut r, ACCUM_LOAD, 0.5);
    acc(&mut r, ACCUM, 0.5);
    acc(&mut r, ACCUM_MULT, 0.5);
    acc(&mut r, ACCUM_ADD, 0.25);
    // Clear the colour, then return the accumulated colour: 0.5 * c + 0.25.
    r.command(Grammar::Gl, SET_COLOUR_CLEAR_VALUE, &[0, 0, 0, 0]);
    r.command(Grammar::Gl, CLEAR_SELECTED_BUFFERS, &[COLOR_BUFFER_BIT]);
    acc(&mut r, ACCUM_RETURN, 1.0);
    let c = colour_at(&mut r, 3, 3);
    // red 0x80 -> 128/255 * 0.5 + 0.25 = 0.502 -> 0x80; green 0x40 -> 0.376 -> 0x60; blue 0 -> 0.25 -> 0x40
    assert_eq!([c >> 16 & 0xff, c >> 8 & 0xff, c & 0xff], [0x80, 0x60, 0x40], "{c:#x}");
}

/// A GL window on the rig's context for the read tests: 64 by 64 at the origin, the back buffer for
/// drawing and reading.
fn read_window(r: &mut Rig) {
    r.x(WINDOW_CLIP_SETUP, &[0, 0, 64, 64, 0, 0, 64, 64, 0, 13, 0]);
    r.command(Grammar::Gl, CONFIGURE_FRAME_BUFFER_LAYOUT, &[0, 8, 0, 1, 0, 0, 24, 0, 0, 0]);
    r.command(Grammar::Gl, SET_VIEWPORT, &[0, 0, 64, 64]);
    r.command(Grammar::Gl, SELECT_CURRENT_MATRIX_STACK, &[PROJECTION]);
    r.command(Grammar::Gl, LOAD_MATRIX_16_FLOATS, &ortho64().map(f32::to_bits));
    r.command(Grammar::Gl, SELECT_DRAW_BUFFER, &[BACK]);
    r.command(Grammar::Gl, SELECT_READ_BUFFER, &[BACK]);
    r.command(Grammar::Gl, SET_RASTER_POSITION_2_COMPONENTS_32_BIT_INTEGER, &[0, 0]);
}

/// Map `pages` pages of host memory at `host` into the device address space from page 8 of the map
/// RAM, and return the device address of byte `at` into them.
fn map_host(r: &mut Rig, host: u64, pages: u64, at: u64) -> u64 {
    for page in 0..pages {
        r.map_ram()[8 + page as usize] = MAPRAM_KIND_16K | ((host + (page << MAPRAM_PAGE_SHIFT)) >> 12) as u32;
    }
    (8u64 << MAPRAM_PAGE_SHIFT) + at
}

/// A read of 16 KiB or more comes inside a select-feed bracket and goes to the bracket's buffer by
/// output DMA: here 64 by 64 RGBA, which is 16384 bytes, 16 over what the return page takes. The
/// bytes reach host memory at the descriptor's address on the finish, which posts event 0x100 with
/// the slot echoed, nothing remaining and no overflow; the return page, and a byte either side of
/// the range, are untouched.
#[test]
fn a_pixel_read_inside_a_bracket_is_delivered_by_output_dma() {
    let mut r = rig();
    read_window(&mut r);
    // Pixel (x, y) is (x, y, x ^ y, 0xff).
    let words: Vec<u32> = (0..64 * 64).map(|i: u32| (i % 64) << 24 | (i / 64) << 16 | ((i % 64) ^ (i / 64)) << 8 | 0xff).collect();
    let hdr = [64, 64, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, 0, 0, 0, 0];
    assert_eq!(r.command(Grammar::Gl, DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, &hdr), Reply::Data(64 * 64));
    r.send(Job::Data { grammar: Grammar::Gl, selector: DRAW_PIXEL_RECTANGLE_FROM_HOST_DATA, args: &hdr, words: &words });
    let host = 0x40_0000u64;
    let len = 64 * 64 * 4u32;
    let dev = map_host(&mut r, host, 2, 0x100);
    r.x(START_SELECT_FEEDBACK_RETURN_DMA, &[(dev >> 12) as u32, 0x9000 | (dev & 0xfff) as u32, len, len, 0, 0]);
    r.put(RETURN_PAGE, &[0; 16]);
    r.command(Grammar::Gl, READ_PIXEL_RECTANGLE_BACK_TO_HOST, &[0, 0, 64, 64, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, 0]);
    assert_eq!(r.bytes(RETURN_PAGE, 16), [0; 16], "nothing on the return page");
    assert_eq!(r.bytes(host + 0x100, 4), [0; 4], "and nothing is written until the finish");
    r.x(FINISH_SELECT_FEEDBACK_WITH_EVENT_CODE, &[0x100, 7]);
    let image = r.bytes(host + 0x100, len as usize);
    for (x, y) in [(0usize, 0usize), (1, 0), (0, 1), (63, 63), (17, 40), (40, 17)] {
        assert_eq!(image[4 * (64 * y + x)..][..4], [x as u8, y as u8, (x ^ y) as u8, 0xff], "pixel ({x}, {y})");
    }
    assert_eq!(r.bytes(host + 0xff, 1), [0], "the byte before the range is untouched");
    assert_eq!(r.bytes(host + 0x100 + len as u64, 4), [0; 4], "and the one after");
    assert_eq!([r.word(REASON_PAGE), r.word(REASON_PAGE + 4), r.word(REASON_PAGE + 8), r.word(REASON_PAGE + 12)], [0x100, 7, 0, 0]);
    // The same size with no bracket has no buffer to go to: answered with zeros, the ready word set.
    r.command(Grammar::Gl, READ_PIXEL_RECTANGLE_BACK_TO_HOST, &[0, 0, 64, 64, FORMAT_RGBA, TYPE_UNSIGNED_BYTE, 0]);
    assert_eq!(r.bytes(RETURN_PAGE, 16), [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
}
