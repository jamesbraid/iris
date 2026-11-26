// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-odyssey/src/dev/odyssey/tests/gap_texture2.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! Texture uploads and sampling, checked in board memory and as GL results.

use crate::dev::odyssey::board::{Board, BoardSnapshot};
use crate::dev::odyssey::consts::{cmd as cmdw, gl_cmd, image, reg, ODSY_USER_CMD_FIFO};
use crate::dev::odyssey::field::level_surface;
use crate::dev::odyssey::frame::pbj;
use crate::dev::odyssey::tests::{board_with_ram, cmd, cmd64, select};
use crate::dev::odyssey::view::{DestViews, Rows, View, MAX_DIM};

const ONE: u32 = 0x3f80_0000;
const RGBA8_LEVEL: u32 = 0x4110_0000;
const UPLOAD_MODE: u32 = 0x0800_24d0;
const YELLOW: u32 = 0xffff_00ff;
const BLUE: u32 = 0x0000_ffff;
const UNTOUCHED: u32 = 0x1234_5678;
const HALF_WHITE: [u32; 4] = [ONE, ONE, ONE, 0x3f00_0000];
const COLOURS: [u32; 4] = [0xff00_00ff, 0x00ff_00ff, 0x0000_ffff, 0xffff_00ff];
const IDENTITY: [u32; 16] = [ONE, 0, 0, 0, 0, ONE, 0, 0, 0, 0, ONE, 0, 0, 0, 0, ONE];

fn board() -> Box<Board> {
    board_with_ram().0
}

fn f(v: f32) -> u32 {
    v.to_bits()
}

fn push(s: &mut Board, words: &[u32]) {
    for &word in words {
        s.mmio_write(ODSY_USER_CMD_FIFO, word as u64, 4);
    }
}

fn set(s: &mut Board, r: u32, v: u32) {
    push(s, &[select(r), v]);
}

/// Each channel within one count.
fn near(got: u32, want: u32) -> bool {
    (0..32)
        .step_by(8)
        .all(|sh| (((got >> sh) & 255) as i32 - ((want >> sh) & 255) as i32).abs() <= 1)
}

/// The library's upload prologue: select the level at `base` as the
/// destination and start an image box of w x h texels.
fn upload_begin(s: &mut Board, base: u32, mode: u32, (wd, h): (u32, u32)) {
    push(
        s,
        &[
            0x0001_1453,
            1,
            0,
            0,
            0,
            0,
            0x0001_0404,
            0,
            0x0001_0405,
            0x2400,
            0x0001_0841,
            0,
            0,
            0x0001_204b,
            0,
            0x00ff_ffff,
            0,
            0,
            0,
            0,
            0,
            0,
            select(reg::DEST_SELECT),
            base,
            2,
            mode,
            0,
            select(reg::WRITE_MASK),
            0x01ff_ffff,
            select(reg::CLIP_BOX),
            wd,
            h,
            0x2900_812f,
            select(reg::GL_BEGIN),
            gl_cmd::PRIMITIVE_IMAGE,
            cmdw::IMAGE_BOX,
            ONE,
            ONE,
            ONE,
            ONE,
            0,
            0,
            0,
            f(wd as f32),
            f(h as f32),
            0,
            gl_cmd::VERTEX_2I,
            0,
            0,
        ],
    );
}

fn upload_end(s: &mut Board) {
    push(
        s,
        &[
            select(reg::GL_END),
            cmdw::WINDOW_RESTORE,
            select(reg::DEST_SELECT),
            0x2000,
            0x50,
            0x2000_6850,
            0,
        ],
    );
}

fn burst_words(s: &mut Board, fmt: u32, words: &[u32]) {
    let n = words.len() as u32;
    push(
        s,
        &[select(
            image::BURST_BASE | (n - n / 16) << cmdw::ARITY_SHIFT | fmt,
        )],
    );
    push(s, words);
}

/// A transfer header, its tail, then one burst of `fmt` words.
fn burst(s: &mut Board, header: u32, fmt: u32, words: &[u32]) {
    push(s, &[cmdw::IMAGE_HEADER, header, cmdw::IMAGE_HEADER_TAIL]);
    burst_words(s, fmt, words);
}

/// A w x h slice of RGBA texels (source code 0x4b), sent in bursts of 16.
fn upload_slice(s: &mut Board, base: u32, pixels: &[u32], (wd, h): (u32, u32)) {
    upload_begin(s, base, UPLOAD_MODE, (wd, h));
    push(
        s,
        &[cmdw::IMAGE_HEADER, 0x0f00_104b, cmdw::IMAGE_HEADER_TAIL],
    );
    for chunk in pixels.chunks(16) {
        burst_words(s, image::FMT_RGB, chunk);
    }
    upload_end(s);
}

/// Texel (x, y) of the level at `base` whose descriptor word 1 is `word1`.
fn level_texel(s: &Board, base: u32, word1: u32, x: u32, y: u32) -> u32 {
    s.gpu_sync();
    let (surface, field) = level_surface(base, word1).unwrap();
    Rows::new(s.vram_ptr(), View::colour(&surface, field, MAX_DIM))
        .load_word(x as i32, y as i32)
        .unwrap_or(0)
}

/// Texel (x, y) of the RGBA8 level at `base`, 2 tiles wide.
fn texel(s: &Board, base: u32, x: u32, y: u32) -> u32 {
    level_texel(s, base, RGBA8_LEVEL | 2, x, y)
}

/// Colour buffer 0 in the layout the last destination select chose.
fn pixel(s: &Board, x: i32, y: i32) -> u32 {
    pixel_in(s, &s.pixel_mode, x, y)
}

fn pixel_in(s: &Board, mode: &[u32; 4], x: i32, y: i32) -> u32 {
    s.gpu_sync();
    let v = DestViews::of(mode, s.pbj(pbj::OVERLAY_BASE), s.pbj(pbj::STRIDE));
    Rows::new(s.vram_ptr(), v.colour[0])
        .load_word(x, y)
        .unwrap_or(0)
}

fn pixel_write(s: &mut Board, x: i32, y: i32, value: u32) {
    s.gpu_sync();
    let v = DestViews::of(&s.pixel_mode, s.pbj(pbj::OVERLAY_BASE), s.pbj(pbj::STRIDE));
    Rows::new(s.vram_ptr(), v.colour[0]).store_word(x, y, value);
}

fn window_transform(s: &mut Board, m: &[u32; 16]) {
    push(
        s,
        &[
            gl_cmd::MATRIX_SELECT_TAG | gl_cmd::MATRIX_SELECT_COMBINED,
            gl_cmd::MATRIX_LOAD,
        ],
    );
    push(s, m);
}

fn block16(s: &mut Board, fourth: u32) {
    push(
        s,
        &[
            gl_cmd::MATRIX_SELECT_TAG | gl_cmd::MATRIX_SELECT_BLOCK16,
            gl_cmd::LOAD_4F,
            0x8000_0000,
            0x8000_0000,
            0x8000_0000,
            fourth,
        ],
    );
}

fn depth_clear(s: &mut Board, rgb: [u32; 3], z: u32) {
    push(
        s,
        &[
            select(reg::GL_BEGIN),
            gl_cmd::QUADS,
            gl_cmd::RECT,
            rgb[0],
            rgb[1],
            rgb[2],
            ONE,
            0,
            0,
            z,
            0x4300_0000,
            0x4300_0000,
            z,
            select(reg::GL_END),
        ],
    );
}

/// The draw window: a 128x128 blue clear under a window transform, an
/// identity texture matrix, the draw write mask and a full-screen clip box.
fn window(s: &mut Board) {
    let m = [
        ONE,
        0,
        0,
        0,
        0,
        ONE,
        0,
        0,
        0,
        0,
        f(4294950912.0),
        f(4294950912.0),
        0,
        0,
        0,
        ONE,
    ];
    window_transform(s, &m);
    block16(s, ONE);
    set(s, reg::WRITE_MASK, 0x01ff_f000);
    depth_clear(s, [0, 0, ONE], 0x4fff_ffc0);
    push(
        s,
        &[
            gl_cmd::MATRIX_SELECT_TAG | gl_cmd::MATRIX_SELECT_TEXTURE,
            gl_cmd::MATRIX_LOAD,
        ],
    );
    push(s, &IDENTITY);
    set(s, reg::WRITE_MASK, 0x00f0_0000);
    push(s, &[select(reg::CLIP_BOX), 1279, 1023]);
}

/// Bind the 2x2 level at `base` (descriptor word 1 `word1`) with sampler
/// word 4 `sampler` and environment mode `env`.
fn bind(s: &mut Board, base: u32, word1: u32, sampler: u32, env: u32) {
    push(
        s,
        &[
            0x0001_0405,
            0x10,
            0x0001_1c16,
            0x0800_0420,
            2,
            2,
            1,
            sampler,
            0,
            0,
            0x0001_141b,
            env,
            0,
            0,
            0,
            0,
            0x0001_0e80,
            base,
            word1,
            0,
            0x0001_0406,
            0,
        ],
    );
}

fn bind_rgba(s: &mut Board, base: u32, sampler: u32, env: u32) {
    bind(s, base, RGBA8_LEVEL | 2, sampler, env);
}

/// A point at pixel (x, y) in `colour` (or white-yellow with `None`) with
/// texture coordinates `coord`, a TEXCOORD word and its operands.
fn point(s: &mut Board, (x, y): (i32, i32), coord: &[u32], colour: Option<[u32; 4]>) {
    match colour {
        Some(c) => push(s, &[gl_cmd::COLOR_4F, c[0], c[1], c[2], c[3]]),
        None => push(s, &[gl_cmd::COLOR_3F, ONE, ONE, 0]),
    }
    push(s, &[select(reg::GL_BEGIN), gl_cmd::POINTS]);
    push(s, coord);
    push(
        s,
        &[
            gl_cmd::VERTEX_2F,
            f(x as f32 + 0.5),
            f(y as f32 + 0.5),
            select(reg::GL_END),
        ],
    );
}

fn point2(s: &mut Board, xy: (i32, i32), st: (f32, f32), colour: Option<[u32; 4]>) {
    point(s, xy, &[gl_cmd::TEXCOORD_2F, f(st.0), f(st.1)], colour);
}

fn point3(s: &mut Board, xy: (i32, i32), str: (f32, f32, f32), colour: Option<[u32; 4]>) {
    point(
        s,
        xy,
        &[gl_cmd::TEXCOORD_3F, f(str.0), f(str.1), f(str.2)],
        colour,
    );
}

/// A 3D texture of `height` x `depth` slices 2 texels wide, slices
/// `stride_units` tiles apart in the level entry, entry word 2 `word2`.
fn bind_3d(
    s: &mut Board,
    base: u32,
    (height, depth): (u32, u32),
    (stride_units, word2): (u32, u32),
    env: u32,
) {
    let size = 0x0800_0000 | (1 << 10) | (height.trailing_zeros() << 5) | depth.trailing_zeros();
    push(
        s,
        &[
            0x0001_0405,
            0x20,
            0x0001_1c16,
            size,
            2,
            height,
            depth,
            0x1300_0000,
            0,
            0,
            0x0001_141b,
            env,
            0,
            0,
            0,
            0,
            0x0001_0e80,
            base,
            RGBA8_LEVEL | stride_units,
            word2,
            0x0001_0406,
            0,
        ],
    );
}

/// The 8x8x8 cube's levels, slice after slice 0x80 units apart: level n has
/// 8 >> n slices of one colour.
fn volume_upload(s: &mut Board) {
    let mut base = 0x16_a800u32;
    let mut size = 8u32;
    for colour in COLOURS {
        let pixels = vec![colour; (size * size) as usize];
        for _ in 0..size {
            upload_slice(s, base, &pixels, (size, size));
            assert_eq!(
                texel(s, base, size - 1, size - 1),
                colour,
                "slice at {base:#x}"
            );
            assert_eq!(texel(s, base, size, 0), 0, "beyond the slice at {base:#x}");
            base += 0x80;
        }
        size >>= 1;
    }
}

/// The cube bound as a mipmapped 3D texture under MODULATE with a white
/// current colour, ready for `volume_quad`.
fn volume_bind(s: &mut Board) {
    window(s);
    push(
        s,
        &[
            0x0001_0404,
            0x0204_2200,
            0x0001_0405,
            0x2420,
            0x0001_1c16,
            0x0800_0c63,
            8,
            8,
            8,
            0x1280_0040,
            0,
            0,
            0x0001_141b,
            2,
            0,
            0,
            0,
            0,
            0x0001_1013,
            0,
            0,
            0x3100,
            0x0f00,
            0x0001_0e80,
            0x16_a800,
            0x4110_0002,
            1,
            0x0001_0406,
            0,
            gl_cmd::COLOR_3F,
            ONE,
            ONE,
            ONE,
        ],
    );
}

/// A screen quad from (32, 32) to (end, end) whose s and t run 0 to `span`
/// and whose r is a half.
fn volume_quad(s: &mut Board, span: u32, end: u32) {
    let m = [
        ONE,
        0,
        0,
        0,
        0,
        0xbf80_0000,
        0,
        0x42ff_fe00,
        0x8000_0000,
        0x8000_0000,
        0x4f7f_ffc0,
        0xcf7f_ffc0,
        0,
        0,
        0,
        ONE,
    ];
    window_transform(s, &m);
    block16(s, 0xbf80_0000);
    push(
        s,
        &[
            0x3300_8000,
            0x2900_80f3,
            0x2a00_8100,
            0x2c00_810c,
            select(reg::GL_BEGIN),
            7,
        ],
    );
    for (x, y, sc, tc) in [
        (32, 32, 0, 0),
        (end, 32, span, 0),
        (end, end, span, span),
        (32, end, 0, span),
    ] {
        push(
            s,
            &[
                gl_cmd::TEXCOORD_3F,
                sc,
                tc,
                0x3f00_0000,
                gl_cmd::VERTEX_2I,
                x,
                y,
            ],
        );
    }
    push(s, &[select(reg::GL_END)]);
}

/// Every pixel of the quad `volume_quad(span, end)` draws.
fn quad_pixels(s: &Board, end: i32) -> Vec<u32> {
    (128 - end..96)
        .flat_map(|y| (32..end).map(move |x| (x, y)))
        .map(|(x, y)| pixel(s, x, y))
        .collect()
}

/// QEMU: sn1/odyssey/texture-adapter/rgba12-byte-upload
#[test]
fn an_rgba_burst_upload_lands_in_the_level_it_selects() {
    let mut s = board();

    upload_begin(&mut s, 0x18_0000, UPLOAD_MODE, (2, 2));
    burst(
        &mut s,
        0x0f00_116b,
        image::FMT_RGB,
        &[0xff00_00ff, 0x00ff_00ff],
    );
    burst(
        &mut s,
        0x0f21_116b,
        image::FMT_RGB,
        &[0x0000_ffff, 0xffff_ffff],
    );
    upload_end(&mut s);
    for (i, want) in [0xff00_00ffu32, 0x00ff_00ff, 0x0000_ffff, 0xffff_ffff]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            texel(&s, 0x18_0000, i as u32 % 2, i as u32 / 2),
            want,
            "texel {i}"
        );
    }
}

/// QEMU: sn1/odyssey/burst-texture-12bit-sources
/// A luminance-alpha texel keeps its alpha, as the DMA path does.
#[test]
fn sixteen_bit_component_bursts_land_as_the_pixel_they_name() {
    let mut s = board();
    let (ci, short) = (image::FMT_CI, image::FMT_RGBA16);

    // The probe's window (origin and one clip rectangle through the kernel
    // port, the captured transform), which the uploads must leave alone.
    for word in [0x0001_0841, 0x100, 0x100, 0x0001_204b] {
        cmd(&mut s, word);
    }
    cmd64(&mut s, 0x0010_0100, 0x0017_f17f);
    for _ in 0..3 {
        cmd64(&mut s, 0x1001, 0);
    }
    let captured = [
        0x4280_0000,
        0,
        0,
        0x4280_0000,
        0,
        0xc280_0000,
        0,
        0x427f_fc00,
        0,
        0,
        0x4f7f_ffc0,
        0x4f7f_ffc0,
        0,
        0,
        0,
        ONE,
    ];
    window_transform(&mut s, &captured);
    block16(&mut s, ONE);

    upload_begin(&mut s, 0x18_0000, UPLOAD_MODE, (4, 1));
    burst(&mut s, 0x0f00_1062, ci, &[0xa68c_870d, 0x1122_ffff]);
    upload_end(&mut s);
    for (x, want) in [0xa6a6_a6ffu32, 0x8787_87ff, 0x1111_11ff, 0xffff_ffff]
        .into_iter()
        .enumerate()
    {
        assert_eq!(texel(&s, 0x18_0000, x as u32, 0), want, "luminance {x}");
    }

    upload_begin(&mut s, 0x18_1000, UPLOAD_MODE, (2, 1));
    burst(&mut s, 0x0f00_1068, ci, &[0x1122_5566, 0xd3d3_7a7a]);
    upload_end(&mut s);
    assert_eq!(texel(&s, 0x18_1000, 0, 0), 0x1111_1155, "luminance-alpha 0");
    assert_eq!(texel(&s, 0x18_1000, 1, 0), 0xd3d3_d37a, "luminance-alpha 1");

    upload_begin(&mut s, 0x18_2000, UPLOAD_MODE, (2, 1));
    burst(
        &mut s,
        0x0f00_1069,
        short,
        &[0x1122_5566, 0x99aa_dfdf, 0xa0a0_0000],
    );
    upload_end(&mut s);
    assert_eq!(texel(&s, 0x18_2000, 0, 0), 0x1155_99ff);
    assert_eq!(texel(&s, 0x18_2000, 1, 0), 0xdfa0_00ff);

    // Two rows of three: one unbounded pixel, a two-pixel group, a row.
    upload_begin(&mut s, 0x18_3000, UPLOAD_MODE, (3, 2));
    burst(&mut s, 0x0f00_106c, short, &[0xffff_2202, 0x3eb4_46fd]);
    push(
        &mut s,
        &[cmdw::IMAGE_HEADER, 0x0f41_106c, cmdw::IMAGE_HEADER_TAIL],
    );
    burst_words(
        &mut s,
        short,
        &[0x8000_0000, 0x0000_ffff, 0x1122_5566, 0x99aa_ddee],
    );
    burst_words(
        &mut s,
        short,
        &[
            0xffff_0000,
            0x0000_ffff,
            0xffff_0000,
            0xffff_0000,
            0xffff_ffff,
            0,
        ],
    );
    upload_end(&mut s);
    for (i, want) in [
        0x473e_22ffu32,
        0xff00_0080,
        0xdd99_5511,
        0xff00_00ff,
        0x00ff_00ff,
        0x0000_ffff,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            texel(&s, 0x18_3000, i as u32 % 3, i as u32 / 3),
            want,
            "texel {i}"
        );
    }
    for (x, y) in [(0, 0), (7, 7), (256, 256)] {
        assert_eq!(pixel_in(&s, &s.pixel_mode, x, y), 0, "screen ({x}, {y})");
    }
}

/// QEMU: sn1/odyssey/texture-adapter/explicit-level
/// Draws with a missing second level or an unknown size code are not checked.
#[test]
fn the_lod_range_picks_which_level_a_draw_samples() {
    let mut s = board();

    upload_slice(&mut s, 0x18_0000, &[0xff00_00ff; 4], (2, 2));
    upload_slice(&mut s, 0x18_1000, &[BLUE], (1, 1));
    assert_eq!(texel(&s, 0x18_0000, 0, 0), 0xff00_00ff);
    assert_eq!(texel(&s, 0x18_1000, 0, 0), BLUE);
    assert_eq!(pixel(&s, 10, 10), 0);
    window(&mut s);
    pixel_write(&mut s, 10, 10, UNTOUCHED);
    push(
        &mut s,
        &[
            0x0001_0405,
            0x10,
            0x0001_1c16,
            0x0800_0420,
            2,
            2,
            1,
            0x1300_0040,
            0,
            0,
            0x0001_141b,
            1,
            0,
            0,
            0,
            0,
            0x0001_0e80,
            0x18_0000,
            0x4110_0002,
            0,
            0x0001_0e81,
            0x18_1000,
            0x4110_0002,
            0,
            0x0001_0406,
            0,
            0x0001_1013,
            0,
            0,
            0x0100,
            0x0f00,
        ],
    );
    point2(&mut s, (10, 10), (0.25, 0.25), None);
    assert_eq!(pixel(&s, 10, 10), BLUE, "a minimum LOD of 1 skips level 0");
    for i in 0..4 {
        push(
            &mut s,
            &[
                0x0001_1c16,
                0x0800_0420,
                2,
                2,
                1,
                0x1300_0040 + i as u32 * 0x20,
                0,
                0,
            ],
        );
        point2(&mut s, (20 + i, 10), (0.25, 0.25), None);
        assert_eq!(pixel(&s, 20 + i, 10), BLUE, "filter code {i}");
    }
    // A zero upper LOD leaves out the second entry.
    push(&mut s, &[0x0001_1013, 0, 0, 0, 0]);
    point2(&mut s, (11, 10), (0.25, 0.25), None);
    assert_eq!(pixel(&s, 11, 10), 0xff00_00ff);
}

/// QEMU: sn1/odyssey/texture-adapter/scale-bias-volume
#[test]
fn the_post_filter_scale_and_bias_apply_to_a_volume_texel() {
    let mut s = board();

    volume_upload(&mut s);
    volume_bind(&mut s);
    window(&mut s);
    push(&mut s, &[0x0001_1013, 0, 0, 0, 0]);
    point3(&mut s, (10, 10), (0.5, 0.5, 0.5), Some([ONE; 4]));
    assert_eq!(pixel(&s, 10, 10), 0xff00_00ff);
    // R scaled 2 biased -1, G -1 and 2, B 0 and 1: level 0's red becomes white.
    push(
        &mut s,
        &[0x0001_203b, 0x2000, 0xf000, 0xf000, 0x2000, 0, 0x1000, 0, 0],
    );
    point3(&mut s, (11, 10), (0.5, 0.5, 0.5), Some([ONE; 4]));
    assert_eq!(pixel(&s, 11, 10), 0xffff_ffff);
}

/// QEMU: sn1/odyssey/texture-adapter/construction
/// Refusals of unknown move templates and mismatched formats are not checked.
#[test]
fn a_texture_move_copies_every_second_texel_into_the_next_level() {
    let mut s = board();
    let pixels = [
        0xff00_00ffu32,
        0,
        BLUE,
        0,
        0,
        0,
        0,
        0,
        0x00ff_00ff,
        0,
        0xffff_ffff,
        0,
        0,
        0,
        0,
        0,
    ];

    upload_slice(&mut s, 0x18_0000, &pixels, (4, 4));
    assert_eq!(texel(&s, 0x18_0000, 0, 0), 0xff00_00ff);
    assert_eq!(texel(&s, 0x18_0000, 2, 2), 0xffff_ffff);
    pixel_write(&mut s, 10, 10, UNTOUCHED);
    let screen = s.pixel_mode;
    push(
        &mut s,
        &[
            select(reg::DEST_SELECT),
            0x18_1000,
            2,
            UPLOAD_MODE,
            0,
            0x0001_0405,
            0x10,
            0x0001_0ebc,
            0x18_0000,
            0x4110_0002,
            0,
            0x0001_0406,
            60,
            0x0001_1c16,
            0x0000_0840,
            4,
            4,
            0,
            0x1364_0000,
            0,
            0,
            select(reg::GL_BEGIN),
            gl_cmd::QUADS,
            gl_cmd::TEX_RECT,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0x4000_0000,
            0x4000_0000,
            0,
            0,
            0,
            0x3f40_0000,
            0x3f40_0000,
            select(reg::GL_END),
        ],
    );
    for (i, want) in [0xff00_00ffu32, BLUE, 0x00ff_00ff, 0xffff_ffff]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            texel(&s, 0x18_1000, i as u32 % 2, i as u32 / 2),
            want,
            "next level texel {i}"
        );
    }
    assert_eq!(
        pixel_in(&s, &screen, 10, 10),
        UNTOUCHED,
        "the screen is not drawn on"
    );
}

/// Draws a 2x2 RGBA8 colour-table texture at four texel centres, SRC_ALPHA
/// over black. The table lanes set each point's colour before blending.
fn blended_colour_table(sampler: u32, rgba: bool, want: [u32; 4]) {
    let table_word = |i: u32| (i * 4095 + 127) / 255;
    let mut s = board();

    upload_slice(
        &mut s,
        0x18_0000,
        &[0xff00_0000, 0x00ff_0040, 0x0000_ff80, 0xffff_ffff],
        (2, 2),
    );
    window(&mut s);
    for plane in 0..2u32 {
        for block in 0..16u32 {
            let words: Vec<u32> = (0..16u32)
                .map(|j| {
                    let i = 16 * block + j;
                    let fourth = table_word(if rgba { 255 - i } else { i });
                    if plane == 1 {
                        table_word(i * i / 255) << 12 | fourth
                    } else {
                        table_word(i / 2) << 12 | table_word(255 - i)
                    }
                })
                .collect();
            push(&mut s, &[0x0001_3e00 | plane << 4 | block]);
            push(&mut s, &words);
        }
    }
    bind_rgba(&mut s, 0x18_0000, sampler, 1);
    depth_clear(&mut s, [0, 0, 0], 0);
    set(&mut s, reg::PATTERN_MODE, 0x0200_3200 | 1 << 17);
    push(&mut s, &[select(reg::BLEND), 0x639, 0, 0]);
    for (i, st) in [(0.25f32, 0.25f32), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
        .into_iter()
        .enumerate()
    {
        point2(&mut s, (20 + i as i32, 10), st, Some([ONE; 4]));
        assert_eq!(
            pixel(&s, 20 + i as i32, 10) & 0xffff_ff00,
            want[i],
            "point {i}"
        );
    }
}

/// QEMU: sn1/odyssey/texture-adapter/colour-table-rgba8-rgb-alpha
#[test]
fn a_blended_rgb_colour_table_lookup_scales_the_looked_up_colour_by_its_alpha() {
    blended_colour_table(0x1350_0001, false, [0, 0, 0x0080_8000, 0x7f00_ff00]);
}

/// QEMU: sn1/odyssey/texture-adapter/colour-table-rgba8-rgba-alpha
#[test]
fn a_blended_rgba_colour_table_lookup_scales_the_looked_up_colour_by_its_alpha() {
    blended_colour_table(0x1360_0001, true, [0x7fff_0000, 0, 0x007f_7f00, 0]);
}

/// QEMU: sn1/odyssey/texture-adapter/scalar-components
/// Decal on these formats is not checked: the library sends it for any format.
#[test]
fn single_component_levels_sample_into_the_components_their_class_names() {
    let mut s = board();
    let (alpha, intensity, luminance) = (0x0000_0002, 0x0020_0002, 0x0040_0002);

    upload_begin(&mut s, 0x18_0000, 0x0800_0020, (2, 2));
    burst(&mut s, 0x0f00_1042, image::FMT_CI, &[0x0040_80c0]);
    upload_end(&mut s);
    for (i, want) in [0x00u32, 0x40, 0x80, 0xc0].into_iter().enumerate() {
        assert_eq!(
            level_texel(&s, 0x18_0000, luminance, i as u32 % 2, i as u32 / 2) & 0xff,
            want,
            "luminance texel {i}"
        );
    }
    window(&mut s);
    // Alpha: the fragment's colour, the texel's alpha (modulated by the fragment's).
    bind(&mut s, 0x18_0000, alpha, 0x1080_0000, 1);
    point2(&mut s, (10, 10), (0.25, 0.25), Some(HALF_WHITE));
    point2(&mut s, (11, 10), (0.75, 0.25), Some(HALF_WHITE));
    assert_eq!(pixel(&s, 10, 10), 0xffff_ff00);
    assert_eq!(pixel(&s, 11, 10), 0xffff_ff40);
    bind(&mut s, 0x18_0000, alpha, 0x1080_0000, 2);
    point2(&mut s, (12, 10), (0.75, 0.25), Some(HALF_WHITE));
    assert_eq!(pixel(&s, 12, 10), 0xffff_ff20);
    // Intensity: the texel in every component.
    bind(&mut s, 0x18_0000, intensity, 0x1200_0000, 1);
    point2(&mut s, (13, 10), (0.25, 0.25), None);
    point2(&mut s, (14, 10), (0.75, 0.25), None);
    assert_eq!(pixel(&s, 13, 10), 0);
    assert_eq!(pixel(&s, 14, 10), 0x4040_4040);
    bind(&mut s, 0x18_0000, intensity, 0x1200_0000, 2);
    point2(&mut s, (15, 10), (0.75, 0.25), Some(HALF_WHITE));
    assert_eq!(pixel(&s, 15, 10), 0x4040_4020);
}

/// A luminance-alpha upload of two rows, each one burst word of two
/// (luminance, alpha) byte pairs.
fn upload_luminance_alpha(s: &mut Board, base: u32, mode: u32, rows: [u32; 2]) {
    upload_begin(s, base, mode, (2, 2));
    for row in rows {
        push(
            s,
            &[cmdw::IMAGE_HEADER, 0x0020_0048, cmdw::IMAGE_HEADER_TAIL],
        );
        burst_words(s, image::FMT_CI, &[row]);
    }
    upload_end(s);
}

/// QEMU: sn1/odyssey/texture-adapter/luminance-alpha
/// Alpha-only headers, 16-bit tags and decal refusals are not checked.
#[test]
fn luminance_alpha_bytes_upload_as_luminance_luminance_luminance_alpha() {
    for (mode, word1) in [(UPLOAD_MODE, RGBA8_LEVEL | 2), (0x0800_0030, 0x00b0_0002)] {
        let mut s = board();

        upload_luminance_alpha(&mut s, 0x18_0000, mode, [0x00ff_ffff, 0xffff_00ff]);
        for (i, want) in [0x0000_00ffu32, 0xffff_ffff, 0xffff_ffff, 0x0000_00ff]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                level_texel(&s, 0x18_0000, word1, i as u32 % 2, i as u32 / 2),
                want,
                "capture texel {i}, mode {mode:#x}"
            );
        }
        upload_luminance_alpha(&mut s, 0x18_1000, mode, [0x1234_5678, 0x9abc_def0]);
        for (i, want) in [0x1212_1234u32, 0x5656_5678, 0x9a9a_9abc, 0xdede_def0]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                level_texel(&s, 0x18_1000, word1, i as u32 % 2, i as u32 / 2),
                want,
                "texel {i}, mode {mode:#x}"
            );
        }
        // One valid pair and a padded pair write one texel.
        upload_begin(&mut s, 0x18_2000, mode, (2, 2));
        push(
            &mut s,
            &[cmdw::IMAGE_HEADER, 0x0010_0048, cmdw::IMAGE_HEADER_TAIL],
        );
        burst_words(&mut s, image::FMT_CI, &[0x1234_dead]);
        upload_end(&mut s);
        assert_eq!(level_texel(&s, 0x18_2000, word1, 0, 0), 0x1212_1234);
        assert_eq!(level_texel(&s, 0x18_2000, word1, 1, 0), 0);
    }
}

/// QEMU: sn1/odyssey/texture-adapter/luminance-alpha
#[test]
fn a_luminance_alpha_level_replaces_with_its_luminance_and_alpha() {
    let mut s = board();

    upload_luminance_alpha(&mut s, 0x18_0000, 0x0800_0030, [0x00ff_ffff, 0xffff_00ff]);
    window(&mut s);
    bind(&mut s, 0x18_0000, 0x00b0_0002, 0x1180_0000, 1);
    for (i, st) in [(0.25f32, 0.25f32), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)]
        .into_iter()
        .enumerate()
    {
        point2(&mut s, (10 + i as i32 % 2, 10 + i as i32 / 2), st, None);
    }
    for (i, want) in [0x0000_00ffu32, 0xffff_ffff, 0xffff_ffff, 0x0000_00ff]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            pixel(&s, 10 + i as i32 % 2, 10 + i as i32 / 2),
            want,
            "pixel {i}"
        );
    }
}

/// A quad over the cube's mip levels after the extra state words `extra`,
/// and the colours of every pixel it draws.
fn volume_draw(extra: &[u32], span: u32, end: u32) -> (Box<Board>, Vec<u32>) {
    let mut s = board();

    volume_upload(&mut s);
    volume_bind(&mut s);
    push(&mut s, extra);
    if extra.len() == 12 {
        pixel_write(&mut s, 64, 64, UNTOUCHED);
    }
    volume_quad(&mut s, span, end);
    let pixels = quad_pixels(&s, end as i32);
    (s, pixels)
}

/// QEMU: sn1/odyssey/texture-adapter/volume-mipmap-trilinear
/// The footprint falls between levels 0 and 1, so the colour is their blend.
#[test]
fn a_trilinear_volume_quad_blends_the_two_levels_its_footprint_falls_between() {
    let (s, pixels) = volume_draw(
        &[0x0001_1c16, 0x0800_0c63, 8, 8, 8, 0x1280_00a0, 0, 0],
        0x4140_0000,
        96,
    );

    assert!(
        pixels.iter().all(|&p| near(p, 0x6a95_00ff)),
        "first off: {:x?}",
        pixels.iter().find(|&&p| !near(p, 0x6a95_00ff))
    );
    assert_eq!(pixel(&s, 31, 64), BLUE, "outside the quad");
}

/// QEMU: sn1/odyssey/texture-adapter/volume-mipmap-depth-one
/// The footprint selects the cube's last level, a single yellow texel.
#[test]
fn a_volume_quad_at_the_footprint_of_the_last_level_reads_that_level() {
    let (s, pixels) = volume_draw(&[], 0x4180_0000, 48);

    assert!(
        pixels.iter().all(|&p| p == YELLOW),
        "first off: {:x?}",
        pixels.iter().find(|&&p| p != YELLOW)
    );
    assert_eq!(pixel(&s, 31, 64), BLUE);
}

/// QEMU: sn1/odyssey/texture-adapter/volume-mipmap-base-level
/// The level entry starts at a later level, so sampling begins there.
#[test]
fn a_volume_whose_level_entry_names_a_later_level_samples_from_there() {
    let extra = [
        0x0001_1c16,
        0x0800_0421,
        2,
        2,
        2,
        0x1280_0040,
        0,
        0,
        0x0001_0e80,
        0x16_ae00,
        0x4110_0002,
        1,
    ];
    let (_, pixels) = volume_draw(&extra, ONE, 96);

    assert!(
        pixels.iter().all(|&p| p == BLUE),
        "first off: {:x?}",
        pixels.iter().find(|&&p| p != BLUE)
    );
}

/// QEMU: sn1/odyssey/texture-adapter/volume-mipmap-nearest
#[test]
fn a_nearest_mip_volume_quad_reads_one_level_and_the_lod_range_pins_each_level() {
    let (mut s, _) = volume_draw(&[], 0x4140_0000, 96);

    assert_eq!(
        pixel(&s, 64, 64),
        COLOURS[1],
        "a 1.5 texel footprint reads level 1"
    );
    assert_eq!(pixel(&s, 31, 64), BLUE);
    assert_eq!(pixel(&s, 96, 64), BLUE);
    window(&mut s);
    for level in 0..4u32 {
        push(&mut s, &[0x0001_1013, 0, 0, level << 8, level << 8]);
        point3(
            &mut s,
            (10 + level as i32, 10),
            (0.99, 0.99, 0.99),
            Some([ONE; 4]),
        );
        assert_eq!(
            pixel(&s, 10 + level as i32, 10),
            COLOURS[level as usize],
            "level {level}"
        );
    }
}

/// QEMU: sn1/odyssey/texture-adapter/volume-slices-follow-each-draw
#[test]
fn a_volume_draw_reads_the_slices_the_texture_state_names_when_it_is_issued() {
    let mut s = board();
    let mut colours = [0u32; 8];

    volume_upload(&mut s);
    // Slice colours distinct, uploaded last slice first.
    for z in (0..8u32).rev() {
        colours[z as usize] = (0x20 * (z + 1)) << 24 | (0x10 * z) << 8 | 0xff;
        upload_slice(
            &mut s,
            0x16_a800 + 0x80 * z,
            &[colours[z as usize]; 64],
            (8, 8),
        );
    }
    let sample = |s: &mut Board, colours: &[u32; 8]| {
        window(s);
        push(s, &[0x0001_1013, 0, 0, 0, 0]);
        for z in 0..8u32 {
            point3(
                s,
                (10 + z as i32, 12),
                (0.5, 0.5, (z as f32 + 0.5) / 8.0),
                Some([ONE; 4]),
            );
            assert_eq!(
                pixel(s, 10 + z as i32, 12),
                colours[z as usize],
                "slice {z}"
            );
        }
    };
    volume_bind(&mut s);
    sample(&mut s, &colours);
    // Slice 3 rewritten with a 16-row upload, then sampled again.
    colours[3] = 0x1234_56ff;
    upload_slice(&mut s, 0x16_a800 + 0x80 * 3, &[colours[3]; 128], (8, 16));
    volume_bind(&mut s);
    sample(&mut s, &colours);
}

/// QEMU: sn1/odyssey/texture-adapter/volume-mipmap-state-restore
/// Memory after a reset is the model's choice, so a reset draw is not checked.
#[test]
fn a_restored_board_draws_a_bound_volume_as_the_original_did() {
    let mut from = board();

    volume_upload(&mut from);
    volume_bind(&mut from);
    let snapshot = BoardSnapshot::capture(&from).unwrap();
    let (mut to, mut control) = (board(), board());
    for copy in [&mut to, &mut control] {
        let (src, dst) = (from.vram.ptr(), copy.vram.ptr());
        for a in (0x16a_0000u64..0x16c_0000).step_by(8) {
            dst.store(a, 8, src.load(a, 8));
        }
    }
    volume_quad(&mut control, 0x4140_0000, 96);
    to.restore_snapshot(snapshot).unwrap();
    volume_quad(&mut to, 0x4140_0000, 96);
    assert_eq!(pixel(&to, 64, 64), 0x00ff_00ff);
    assert_ne!(
        pixel(&control, 64, 64),
        0x00ff_00ff,
        "without the state the quad is not textured"
    );
}

/// QEMU: sn1/odyssey/texture-adapter/level-zero-3d
#[test]
fn a_volume_draw_reads_the_slice_its_r_coordinate_selects() {
    let mut s = board();
    let slice_colours = [0xff00_00ffu32, 0x00ff_00ff, BLUE, 0xffff_ffff];

    for (height, word2, spacing) in [(2u32, 1u32, 0x80u32), (32, 2, 0x100)] {
        for z in 0..4u32 {
            upload_slice(
                &mut s,
                0x19_0000 + z * spacing,
                &vec![slice_colours[z as usize]; (2 * height) as usize],
                (2, height),
            );
            assert_eq!(
                texel(&s, 0x19_0000 + z * spacing, 0, 0),
                slice_colours[z as usize]
            );
        }
        window(&mut s);
        bind_3d(&mut s, 0x19_0000, (height, 4), (2, word2), 1);
        for z in 0..4i32 {
            point3(
                &mut s,
                (10 + z, 10),
                (0.25, 0.25, (z as f32 + 0.5) / 4.0),
                None,
            );
            assert_eq!(
                pixel(&s, 10 + z, 10),
                slice_colours[z as usize],
                "{height} high, slice {z}"
            );
        }
    }
    // A one-slice volume still samples as a volume: r 1.75 wraps to slice 0.
    bind_3d(&mut s, 0x19_0000, (32, 1), (2, 2), 1);
    pixel_write(&mut s, 18, 10, UNTOUCHED);
    point3(&mut s, (18, 10), (0.25, 0.25, 1.75), None);
    assert_eq!(pixel(&s, 18, 10), slice_colours[0]);
}

/// QEMU: sn1/odyssey/texture-adapter/level-zero-3d
/// DECAL and BLEND expectations follow GL; the board's own result is unknown.
#[test]
fn a_volume_texel_modulates_decals_and_blends_the_fragment_colour_as_gl_does() {
    let mut s = board();
    let colour = [f(0.5), f(0.25), f(0.75), f(0.5)];

    for z in 0..4 {
        upload_slice(&mut s, 0x19_0000 + z * 0x100, &[0xffff_ffff; 64], (2, 32));
    }
    window(&mut s);
    for (env, want) in [(2u32, 0x8040_c080u32), (3, 0xffff_ff80), (4, 0x0000_0080)] {
        let x = 14 + 2 * env as i32;
        bind_3d(&mut s, 0x19_0000, (32, 4), (2, 2), env);
        point3(&mut s, (x, 10), (0.25, 0.25, 0.875), Some(colour));
        let got = pixel(&s, x, 10);
        assert!(
            near(got, want),
            "env {env}: {got:#010x}, want {want:#010x} within a count"
        );
    }
}

/// QEMU: sn1/odyssey/texture-adapter/linear-strq
/// QEMU: sn1/odyssey/texture-adapter/linear-srq
#[test]
fn a_quad_with_linear_texture_planes_reads_the_texel_the_planes_name() {
    for t_enabled in [true, false] {
        let mut s = board();
        let mut pixels = [[0u32; 4]; 4];
        for z in 0..4u32 {
            for t in 0..2u32 {
                for r in 0..2u32 {
                    pixels[z as usize][(2 * t + r) as usize] = (16 + 48 * z + 24 * t + 12 * r)
                        << 24
                        | (32 + 48 * r) << 16
                        | (48 + 64 * t) << 8
                        | 0xff;
                }
            }
            upload_slice(&mut s, 0x16_a800 + z * 0x80, &pixels[z as usize], (2, 2));
        }
        window(&mut s);
        let m = [
            ONE,
            0,
            0,
            0,
            0,
            0xbf80_0000,
            0,
            0x42ff_fe00,
            0x8000_0000,
            0x8000_0000,
            0x4f7f_ffc0,
            0xcf7f_ffc0,
            0,
            0,
            0,
            ONE,
        ];
        window_transform(&mut s, &m);
        block16(&mut s, 0xbf80_0000);
        bind_3d(&mut s, 0x16_a800, (2, 4), (2, 1), 1);
        if t_enabled {
            push(
                &mut s,
                &[0xc380_d025, 0x3f40_0000, 0x3e80_0000, 0x3f20_0000, ONE],
            );
            push(&mut s, &SETUP_STRQ);
        } else {
            push(
                &mut s,
                &[0xc380_d025, 0x3e00_0000, 0x3f40_0000, 0x3f60_0000, ONE],
            );
            push(
                &mut s,
                &[0x3100_8104, 0x9080_91bb, 0, 0x3c80_0000, 0, 0xbf00_0000],
            );
            push(&mut s, &SETUP_SRQ);
        }
        push(
            &mut s,
            &[
                0x0001_4400,
                7,
                0x8080_c800,
                32,
                32,
                0x8080_c800,
                96,
                32,
                0x8080_c800,
                96,
                96,
                0x8080_c800,
                32,
                96,
                0x0001_4001,
            ],
        );
        let mut off = Vec::new();
        for y in 32..96i32 {
            for x in 32..96i32 {
                let (px, py) = (x as f64 + 0.5, 127.99609375 - (y as f64 + 0.5));
                let along = if t_enabled { px } else { py };
                let q = along / 128.0 + 0.5;
                let sc = (if t_enabled {
                    px / 64.0 - 0.5
                } else {
                    px / 32.0 - 1.0
                }) / q;
                let tc = (if t_enabled { py / 64.0 - 0.5 } else { 0.75 }) / q;
                let rc = (along / 32.0 - 1.0) / q;
                let (si, ti, ri) = (
                    (sc * 2.0).floor() as i64,
                    (tc * 2.0).floor() as i64,
                    (rc * 4.0).floor() as i64,
                );
                let want = pixels[ri.rem_euclid(4) as usize]
                    [(2 * ti.rem_euclid(2) + si.rem_euclid(2)) as usize];
                if pixel(&s, x, y) != want {
                    off.push((x, y));
                }
            }
        }
        assert!(
            off.is_empty(),
            "STRQ {t_enabled}: {} of 4096 pixels off, first {:?}",
            off.len(),
            off.first()
        );
    }
}

const SETUP_STRQ: [u32; 53] = [
    0x2800_8000,
    0x6000_80f3,
    0x2800_8001,
    0x6000_810c,
    0x2800_8002,
    0x6000_8033,
    0x2800_8003,
    0x6000_804d,
    0x2800_8004,
    0x6000_8057,
    0x2800_8005,
    0x6000_8061,
    0x2800_8006,
    0x6000_8066,
    0x2800_8007,
    0x6000_806c,
    0x2800_8008,
    0x6000_8077,
    0x2800_8009,
    0x6080_809a,
    0x2800_800a,
    0x6000_8203,
    0x2800_800b,
    0x6080_80c5,
    0x3100_8100,
    0x9080_91bb,
    0x3c80_0000,
    0,
    0,
    0xbf00_0000,
    0x3100_8104,
    0x9080_91bb,
    0,
    0x3c80_0000,
    0,
    0xbf00_0000,
    0x3100_8108,
    0x9080_91bb,
    0x3d00_0000,
    0,
    0,
    0xbf80_0000,
    0x3100_810c,
    0x9080_91bb,
    0x3c00_0000,
    0,
    0,
    0x3f00_0000,
    0x3100_8000,
    0x3300_8001,
    0x2900_80f3,
    0x2a00_8100,
    0x2c00_810c,
];

const SETUP_SRQ: [u32; 46] = [
    0x2800_8000,
    0x6000_80f3,
    0x2800_8001,
    0x6000_810c,
    0x2800_8002,
    0x6000_8033,
    0x2800_8003,
    0x6000_804d,
    0x2800_8004,
    0x6000_8061,
    0x2800_8005,
    0x6000_8066,
    0x2800_8006,
    0x6000_806c,
    0x2800_8007,
    0x6000_8077,
    0x2800_8008,
    0x6080_809a,
    0x2800_8009,
    0x6000_8203,
    0x2800_800a,
    0x6080_80c5,
    0x3100_8100,
    0x9080_91bb,
    0x3d00_0000,
    0,
    0,
    0xbf80_0000,
    0x3100_8108,
    0x9080_91bb,
    0,
    0x3d00_0000,
    0,
    0xbf80_0000,
    0x3100_810c,
    0x9080_91bb,
    0,
    0x3c00_0000,
    0,
    0x3f00_0000,
    0x3100_8000,
    0x3300_800a,
    0x3300_8001,
    0x2900_80f3,
    0x2a00_8100,
    0x2c00_810c,
];
