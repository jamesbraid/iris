// SPDX-License-Identifier: BSD-3-Clause
// Ported from rust/sgi-odyssey/src/dev/odyssey/tests/gap_other.rs in jamesbraid/qemu-sgi-origami, branch gfx-engines.
//! Behaviour the old Odyssey qtest pinned through log lines, shell
//! properties and the old work queue, asserted on the engine instead.

use crate::dev::odyssey::board::{
    Board, BoardSnapshot, ODSY_BOOT_HEIGHT, ODSY_BOOT_WIDTH, ODSY_HEIGHT, ODSY_WIDTH,
};
use crate::dev::odyssey::clip::ClipRect;
use crate::dev::odyssey::consts::{
    ctx, dispmode, dma, gl_cmd, light, reg, tex, write, ODSY_GAMMA_GROUP, ODSY_USER_CMD_FIFO,
};
use crate::dev::odyssey::dma::{DMA_BPP, ODSY_CMD_IDLE};
use crate::dev::odyssey::pipe::encode::{vtx, LibDraw, GL_FLAT, GL_QUADS};
use crate::dev::odyssey::pipe::state::StencilOp;
use crate::dev::odyssey::raster_bench::{put_texel, texel_format_word, Bench};
use crate::dev::odyssey::tests::dma::{descriptor, pte, put_table, PAGE_A, TABLE};
use crate::dev::odyssey::tests::{
    board, board_with_ram, cmd, pixel, pixel_write, screen_field, select, Ram,
};
use std::sync::Arc;

const SHADOW: u64 = 0x2000;
const SYNC: u64 = SHADOW + 0x800;

fn user(s: &mut Board, word: u32) {
    s.mmio_write(ODSY_USER_CMD_FIFO, word as u64, 4);
}

fn user_words(s: &mut Board, words: &[u32]) {
    for &w in words {
        user(s, w);
    }
}

fn word(ram: &Ram, at: u64) -> u32 {
    u32::from_be_bytes(ram.get(at, 4).try_into().unwrap())
}

fn query_board() -> (Box<Board>, Arc<Ram>) {
    let (mut s, _, ram) = board_with_ram();
    s.widget.ctx_shadow_addr_lower = SHADOW as u32;
    s.widget.host_write_addr_lower = SYNC as u32;
    (s, ram)
}

/// Checks the record, the words after it and the count and sync half-word
/// the query publishes.
fn texture_query(s: &mut Board, ram: &Ram, record: &[u32], count: u32, slot: u32, seq: u32) {
    ram.put(SHADOW, &[0xa5; 4096]);
    for w in [0x4c0, count, 0x10a5a, slot, seq] {
        cmd(s, w);
    }
    assert_eq!(
        word(ram, SHADOW + 0x94c),
        0xa5a5_a5a5,
        "the word before the record is untouched"
    );
    for (i, &w) in record.iter().enumerate() {
        assert_eq!(
            word(ram, SHADOW + 0x950 + 4 * i as u64),
            w,
            "record word {i}"
        );
    }
    for i in record.len()..6 {
        assert_eq!(
            word(ram, SHADOW + 0x950 + 4 * i as u64),
            0xa5a5_a5a5,
            "word {i} past the record"
        );
    }
    assert_eq!(word(ram, SHADOW + 0x97c), count);
    assert_eq!(ram.half(SYNC + 2 * slot as u64), seq as u16);
}

const TEX_4F: [[u32; 5]; 2] = [
    [
        gl_cmd::TEXCOORD_4F,
        0x3f40_0000,
        0x3e80_0000,
        0x3f20_0000,
        0x3f80_0000,
    ],
    [
        gl_cmd::TEXCOORD_4F,
        0x3e00_0000,
        0x3f40_0000,
        0x3f60_0000,
        0x3f80_0000,
    ],
];
const TEX_3F: [[u32; 4]; 2] = [
    [gl_cmd::TEXCOORD_3F, 0x3f40_0000, 0x3e80_0000, 0x3f20_0000],
    [gl_cmd::TEXCOORD_3F, 0x3e00_0000, 0x3f40_0000, 0x3f60_0000],
];
const TEX_1F: [[u32; 2]; 2] = [
    [gl_cmd::TEXCOORD_1F, 0x3f40_0000],
    [gl_cmd::TEXCOORD_1F, 0x3e00_0000],
];
const TEX_DEFAULT: [u32; 3] = [gl_cmd::TEXCOORD_2F, 0, 0];

/// Both ports reach the same record: the user FIFO and the kernel's
/// (which replays a saved record at a context restore).
fn send(s: &mut Board, words: &[u32], kernel: bool) {
    for &w in words {
        if kernel {
            cmd(s, w);
        } else {
            user(s, w);
        }
    }
}

/// Each setter form answers at +0x950 from either port, and the newest wins.
/// An unanswered query leaves the library polling forever.
///
/// QEMU: texture-current-query-4f, texture-current-query-3f,
/// texture-current-query-3f-kernel, texture-current-query-1f,
/// texture-current-query-1f-kernel.
#[test]
fn the_current_texture_query_answers_each_setter_form() {
    let forms: [[&[u32]; 2]; 3] = [
        [&TEX_4F[0], &TEX_4F[1]],
        [&TEX_3F[0], &TEX_3F[1]],
        [&TEX_1F[0], &TEX_1F[1]],
    ];
    for records in forms {
        for kernel in [false, true] {
            let (mut s, ram) = query_board();
            for (owner, record) in records.iter().enumerate() {
                send(&mut s, record, kernel);
                texture_query(&mut s, &ram, record, 2, if owner == 1 { 8 } else { 5 }, 1);
            }
        }
    }
}

/// An integer coordinate is a current-value record like the float ones,
/// so the kernel can replay it.
///
/// QEMU: texture-current-query-1i.
#[test]
fn the_current_texture_query_answers_an_integer_coordinate_as_sent() {
    let (mut s, ram) = query_board();
    let record = [gl_cmd::TEXCOORD_1I, 0xffff_fffb];

    send(&mut s, &record, false);
    texture_query(&mut s, &ram, &record, 2, 5, 1);
}

/// Before any coordinate is sent, and after a reset, the record is the GL
/// initial value.
///
/// QEMU: texture-current-query-4f (the reset at its end).
#[test]
fn the_current_texture_query_after_a_reset_answers_the_initial_coordinate() {
    let (mut s, ram) = query_board();

    texture_query(&mut s, &ram, &TEX_DEFAULT, 2, 5, 1);
    send(&mut s, &TEX_4F[0], false);
    s.reset();
    s.widget.ctx_shadow_addr_lower = SHADOW as u32;
    s.widget.host_write_addr_lower = SYNC as u32;
    texture_query(&mut s, &ram, &TEX_DEFAULT, 2, 5, 1);
}

/// The loaded snapshot's record replaces the destination's, even a default
/// one.
///
/// QEMU: texture-current-query-4f-migration,
/// texture-current-query-4f-migration-defaults,
/// texture-current-query-3f-migration, texture-current-query-1f-migration.
#[test]
fn a_loaded_snapshot_brings_its_current_texture_record() {
    let saved: [&[u32]; 4] = [&TEX_4F[1], &TEX_3F[1], &TEX_1F[1], &TEX_DEFAULT];

    for record in saved {
        let (mut from, _) = query_board();
        if record != TEX_DEFAULT {
            send(&mut from, record, false);
        }
        let snapshot = BoardSnapshot::capture(&from).unwrap();

        let (mut to, ram) = query_board();
        send(&mut to, &TEX_4F[0], false);
        to.restore_snapshot(snapshot).unwrap();
        to.widget.ctx_shadow_addr_lower = SHADOW as u32;
        to.widget.host_write_addr_lower = SYNC as u32;
        texture_query(&mut to, &ram, record, 2, 8, 2);
    }
}

/// The copy-rectangle prefix is a plain register word, so the next word is
/// parsed as a command, not as blit operands.
///
/// QEMU: command-fifo-copyrect-prefix-is-a-register.
#[test]
fn the_copy_rect_prefix_is_a_plain_register_word() {
    let (mut s, _) = board("v12");

    cmd(&mut s, select(reg::BLIT_PREFIX));
    cmd(&mut s, 0x0012_0000);
    assert_eq!(s.cmd.state, ODSY_CMD_IDLE);
    assert_eq!(
        s.diag.reported_cmd_count, 0,
        "the prefix and its operand are known"
    );

    cmd(&mut s, 0x0000_7777);
    assert_eq!(s.diag.reported_cmd_count, 1);
    assert_eq!(s.diag.reported_cmd[0], 0x7777);
}

/// A command word inside a query runs as a command, and the cut query
/// publishes nothing.
///
/// QEMU: command-fifo-compare-cancels-query-user.
#[test]
fn a_command_word_inside_a_query_cancels_it() {
    let (mut s, ram) = query_board();
    ram.put(SHADOW, &[0xa5; 4096]);

    // A whole query for control.
    for w in [0x4c0, 0x1122_3344, 0x10a5a, 2, 0x2222] {
        user(&mut s, w);
    }
    assert_eq!(word(&ram, SHADOW + 0x97c), 0x1122_3344);
    ram.put(SHADOW, &[0xa5; 4096]);

    // The same, with an id-compare register write where the sync word goes.
    for w in [0x4c0, 0x1234_5678, select(reg::ID_COMPARE), 1, 0] {
        user(&mut s, w);
    }
    for w in [select(reg::HOST_WRITE), 3, 0x4321] {
        user(&mut s, w);
    }
    assert_eq!(
        word(&ram, SHADOW + 0x97c),
        0xa5a5_a5a5,
        "the cut query published nothing"
    );
    assert_eq!(
        ram.half(SYNC + 6),
        0x4321,
        "the words after it ran as the fence command they are"
    );

    for w in [0x4c0, 0xabcd_ef01, 0x10a5a, 4, 0x5432] {
        user(&mut s, w);
    }
    assert_eq!(word(&ram, SHADOW + 0x97c), 0xabcd_ef01);
    assert_eq!(ram.half(SYNC + 8), 0x5432);
}

fn prog(class: u32, v: u32) -> u32 {
    class << 24 | 0x8000 | (v & 0x3ff) | ((v >> 10) & 1) << 23
}

/// The replay's operands are zeros and floats that look like commands, so
/// each word must take them as operands.
///
/// QEMU: command-fifo-context-replay-words.
#[test]
fn the_context_replay_words_take_their_operands() {
    let (mut s, _) = board("v12");

    for index in (0..512).step_by(16) {
        if index == 320 || index == 480 {
            continue;
        }
        cmd(&mut s, prog(0x31, index));
        cmd(&mut s, 0x9080_bda2);
        for _ in 0..16 {
            cmd(&mut s, 0x0000_7777);
        }
    }
    cmd(&mut s, prog(0x34, 0));
    cmd(&mut s, 0x9080_bdc8);
    for _ in 0..16 {
        cmd(&mut s, 0x3f80_0000);
    }
    cmd(&mut s, prog(0x34, 16));
    cmd(&mut s, 0x9080_91df);
    for _ in 0..4 {
        cmd(&mut s, 0x0000_7777);
    }
    for entry in [36, 52, 20] {
        cmd(&mut s, prog(0x34, entry));
        cmd(&mut s, 0x9080_bdc8);
        for i in 0..16 {
            cmd(&mut s, if i % 5 != 0 { 0 } else { 0x3f80_0000 });
        }
    }
    cmd(&mut s, prog(0x28, 0));
    for i in 0..40 {
        cmd(&mut s, prog(0x60, i));
    }
    for w in [
        0x9080_91ea,
        0x0000_7777,
        0x8000_7777,
        0x0000_7778,
        0x8000_7778,
    ] {
        cmd(&mut s, w);
    }
    assert_eq!(s.cmd.state, ODSY_CMD_IDLE);
    assert_eq!(
        s.diag.reported_cmd_count, 0,
        "no operand was read as a command"
    );
}

fn switch_out(s: &mut Board) {
    s.mmio_write(ctx::SWITCH_PORT, 7 << 40 | 1, 8);
}

/// The library's near-plane fallback reads the strip's bit 11 parity flag
/// to replay held vertices in order.
///
/// QEMU: context-save-strip-parity.
#[test]
fn an_open_strips_saved_state_flags_an_odd_next_triangle() {
    for n in 2..=5u32 {
        let (mut s, ram) = query_board();

        user(&mut s, select(reg::GL_BEGIN));
        user(&mut s, gl_cmd::TRIANGLE_STRIP);
        for i in 0..n {
            user_words(
                &mut s,
                &[
                    gl_cmd::VERTEX_2F,
                    if i & 1 != 0 { 0x3f00_0000 } else { 0xbf00_0000 },
                    i << 22,
                ],
            );
        }
        switch_out(&mut s);

        let state = word(&ram, SHADOW + 0x80);
        assert_eq!(state & 0xf, 5, "{n} vertices: the strip");
        assert_eq!(
            state & (1 << 11) != 0,
            n & 1 != 0,
            "{n} vertices: state {state:#x}"
        );
    }
}

/// Operation codes the library never sends (6, 7) must not fold into a
/// defined operation.
///
/// QEMU: command-fifo-gl-stencil-state.
#[test]
fn the_stencil_word_decodes_its_fields_and_keeps_undefined_operations_apart() {
    use crate::dev::odyssey::pipe::state::Compare;
    let (mut s, _) = board("v12");

    // ALWAYS, REPLACE on fail, depth fail and pass, mask 0xff, reference 0.
    cmd(&mut s, select(reg::STENCIL_FUNC));
    cmd(&mut s, 0x0e92_ff00);
    let w = s.fragment_words().backend;
    assert_eq!(w.stencil_func(), Compare::Always);
    assert_eq!(w.stencil_ops(), [StencilOp::Replace; 3]);
    assert_eq!(w.stencil_value_mask(), 0xff);
    assert_eq!(w.stencil_ref(), 0);

    for (code, op) in [
        (0, StencilOp::Keep),
        (1, StencilOp::Zero),
        (2, StencilOp::Replace),
        (3, StencilOp::Incr),
        (4, StencilOp::Decr),
        (5, StencilOp::Invert),
        (6, StencilOp::Unknown(6)),
        (7, StencilOp::Unknown(7)),
    ] {
        cmd(&mut s, select(reg::STENCIL_FUNC));
        cmd(&mut s, 0x0e92_ff00 & !(7 << 22) | code << 22);
        assert_eq!(
            s.fragment_words().backend.stencil_ops(),
            [op, StencilOp::Replace, StencilOp::Replace],
            "fail op {code}"
        );
    }
}

// The GL probe window: a 128 x 128 window at (256, 256) with one clip
// rectangle.
fn probe_window(s: &mut Board) {
    for w in [0x10841, 0x100, 0x100, 0x1204b] {
        cmd(s, w);
    }
    for q in [
        0x0010_0100_0017_f17f,
        0x1001_0000_0000,
        0x1001_0000_0000,
        0x1001_0000_0000,
    ] {
        s.mmio_write(crate::dev::odyssey::consts::cmd::FIFO, q, 8);
    }
    user_words(
        s,
        &[
            0x3400_8000,
            0x9080_bdc8,
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
            0x3f80_0000,
            0x3400_8010,
            0x9080_91df,
            0x8000_0000,
            0x8000_0000,
            0x8000_0000,
            0x3f80_0000,
        ],
    );
}

const MODE_FLAT: u32 = 0x0200_3200;
const MODE_BLEND: u32 = 1 << 17;
const DEPTH_2S: u32 = 0x4fff_ffc0;
const ONE: u32 = 0x3f80_0000;
const YELLOW: u32 = 0xffff_00ff;
const RED: u32 = 0xff00_00ff;
const BLUE: u32 = 0x0000_ffff;

fn set_reg(s: &mut Board, r: u32, operands: &[u32]) {
    user(s, select(r));
    user_words(s, operands);
}

/// A colour and depth clear of the window: the library's rectangle.
fn clear(s: &mut Board, rgb: [u32; 3], z: u32) {
    set_reg(
        s,
        reg::GL_BEGIN,
        &[
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
        ],
    );
    user(s, select(reg::GL_END));
}

fn draw(s: &mut Board, prim: u32, rgb: [u32; 3], vertices: &[[u32; 3]]) {
    user_words(s, &[gl_cmd::COLOR_3F, rgb[0], rgb[1], rgb[2]]);
    set_reg(s, reg::GL_BEGIN, &[prim]);
    for v in vertices {
        user_words(s, &[gl_cmd::VERTEX_3F, v[0], v[1], v[2]]);
    }
    user(s, select(reg::GL_END));
}

fn count(s: &Board, colour: u32, (x0, x1): (i32, i32), (y0, y1): (i32, i32)) -> usize {
    (y0..=y1)
        .flat_map(|y| (x0..=x1).map(move |x| (x, y)))
        .filter(|&(x, y)| pixel(s, 0, x, y) == colour)
        .count()
}

const HORIZONTAL: [[u32; 3]; 2] = [[0xbf00_0000, 0, 0xbf00_0000], [0x3f00_0000, 0, 0xbf00_0000]];
const CENTRE: [[u32; 3]; 1] = [[0, 0, 0xbf00_0000]];

/// Line width and point size are fixed point, 128 to the pixel.
///
/// QEMU: command-fifo-gl-line-point-size.
#[test]
fn line_width_and_point_size_are_whole_pixels_in_the_registers_fixed_point() {
    let (mut s, _) = board("v12");
    probe_window(&mut s);
    set_reg(&mut s, reg::PATTERN_MODE, &[MODE_FLAT]);

    clear(&mut s, [0, 0, 0], 0);
    draw(&mut s, gl_cmd::LINES, [ONE, ONE, 0], &HORIZONTAL);
    assert_eq!(
        count(&s, YELLOW, (320, 320), (314, 326)),
        1,
        "a default line is one row"
    );
    clear(&mut s, [0, 0, 0], 0);
    draw(&mut s, gl_cmd::POINTS, [ONE, ONE, 0], &CENTRE);
    assert_eq!(
        count(&s, YELLOW, (316, 323), (316, 323)),
        1,
        "a default point is one pixel"
    );

    set_reg(&mut s, reg::LINE_POINT_SIZE, &[0x180, 0x80]);
    assert_eq!(s.fragment_words().raster.line_width_x128(), 0x180);
    clear(&mut s, [0, 0, 0], 0);
    draw(&mut s, gl_cmd::LINES, [ONE, ONE, 0], &HORIZONTAL);
    assert_eq!(
        count(&s, YELLOW, (320, 320), (314, 326)),
        3,
        "width 3.0 is three rows"
    );

    set_reg(&mut s, reg::LINE_POINT_SIZE, &[0x80, 0x200]);
    assert_eq!(s.fragment_words().raster.point_size_x128(), 0x200);
    clear(&mut s, [0, 0, 0], 0);
    draw(&mut s, gl_cmd::POINTS, [ONE, ONE, 0], &CENTRE);
    assert_eq!(
        count(&s, YELLOW, (316, 323), (316, 323)),
        16,
        "size 4.0 is a 4 x 4 square"
    );
}

/// Only smoothing blends partial coverage; aliased lines leave no such pixels.
///
/// QEMU: command-fifo-gl-line-smoothing.
#[test]
fn a_smooth_line_blends_partial_coverage_and_an_aliased_one_does_not() {
    use crate::dev::odyssey::consts::mode;
    const DIAGONAL: [[u32; 3]; 2] = [
        [0xbe80_0000, 0xbe80_0000, 0xbf00_0000],
        [0x3e80_0000, 0x3e80_0000, 0xbf00_0000],
    ];
    let (mut s, _) = board("v12");
    probe_window(&mut s);
    set_reg(&mut s, reg::BLEND, &[0x639, 0, 0]);
    set_reg(&mut s, reg::LINE_POINT_SIZE, &[0x80, 0x80]);

    let partial = |s: &Board| {
        (296..=344)
            .flat_map(|y| (296..=344).map(move |x| (x, y)))
            .filter(|&(x, y)| ![BLUE, RED].contains(&pixel(s, 0, x, y)))
            .count()
    };
    for (bits, smooth) in [(0, false), (mode::LINE_SMOOTH, true), (0, false)] {
        set_reg(&mut s, reg::PATTERN_MODE, &[MODE_FLAT | MODE_BLEND | bits]);
        clear(&mut s, [0, 0, ONE], 0x4fff_ffc0);
        draw(&mut s, gl_cmd::LINES, [ONE, 0, 0], &DIAGONAL);
        assert_eq!(partial(&s) > 0, smooth, "smooth bits {bits:#x}");
    }
}

/// The eye position for lighting comes from the model-view, so a translation
/// moves the lit colour.
///
/// QEMU: command-fifo-gl-lighting-uses-modelview.
#[test]
fn lighting_takes_the_eye_position_from_the_model_view() {
    let (mut s, _) = board("v12");
    let identity: [f32; 16] = [
        1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
    ];
    let translate: [f32; 16] = [
        1., 0., 0., 1., 0., 1., 0., 1., 0., 0., 1., 2., 0., 0., 0., 1.,
    ];
    let (light_at, diffuse) = ([1.0f32, 2.0, 3.0], [1.0f32, 0.5, 0.25]);

    s.geom.normal_matrix = identity.map(f32::to_bits);
    s.geom.normal = [0.0f32, 0.0, 1.0].map(f32::to_bits);
    for k in 0..3 {
        s.geom.light_colour[4 + k] = diffuse[k].to_bits();
        s.geom.light_vec[light::POS + k] = light_at[k].to_bits();
    }
    s.geom.light_vec[light::ATTEN] = 1.0f32.to_bits();
    s.geom.light_vec[light::SPOT_COS] = 1.0f32.to_bits();
    for k in 0..4 {
        s.geom.material[k] = 0.0f32.to_bits();
        s.geom.material[4 + k] = 1.0f32.to_bits();
        s.geom.material[8 + k] = 0.0f32.to_bits();
        s.geom.material[12 + k] = 0.0f32.to_bits();
    }
    let origin = [0.0f32, 0.0, 0.0, 1.0];

    // n . L over |L|, with n = +z, for the vertex at `eye`.
    let ndotl = |eye: [f32; 3]| {
        let l: Vec<f64> = (0..3).map(|k| (light_at[k] - eye[k]) as f64).collect();
        (l[2] / l.iter().map(|v| v * v).sum::<f64>().sqrt()).max(0.0)
    };
    let mut lit = Vec::new();
    for (modelview, eye) in [(identity, [0.0f32; 3]), (translate, [1.0, 1.0, 2.0])] {
        s.geom.modelview = modelview.map(f32::to_bits);
        let (front, _) = s.gl_light(&origin);
        let want = ndotl(eye);
        for k in 0..3 {
            let expect = want * diffuse[k] as f64;
            assert!(
                (front[k] as f64 - expect).abs() < 1e-4,
                "eye {eye:?} channel {k}: {front:?} against {expect}"
            );
        }
        lit.push(front);
    }
    assert!(
        (lit[0][0] - lit[1][0]).abs() > 0.05,
        "the translation must move the lit colour"
    );
}

/// Alpha of the centre pixel of a textured quad over a uniform texture.
fn textured_alpha(components: u32, env: u32) -> u32 {
    let mut b = Bench::empty();
    b.board.gl_write_mask |= write::COLOUR_MASK << write::COLOUR_SHIFT;
    b.st.shade_model = GL_FLAT;
    let word1 = texel_format_word(components) | 2;
    for y in 0..4 {
        for x in 0..4 {
            put_texel(&b.board, 0x10_0000, word1, x, y, 0x8040_2080);
        }
    }
    b.board.tex_mut().levels[..3].copy_from_slice(&[0x10_0000, word1, 0]);
    b.board.tex_mut().path = tex::MODE_2D;
    b.board.tex_mut().level0 = 0;
    b.board.tex_mut().sampler = [
        0x0800_0000 | 2 << 10 | 2 << 5,
        4,
        4,
        1,
        0x1000_0000 | components,
        0,
        0,
    ];
    b.board.tex_mut().env = [env, 0, 0, 0, 0];
    b.st.texture_2d = true;

    let ndc = |p: f32, size: u32| p / size as f32 * 2.0 - 1.0;
    let mut v = vec![0.0f32; 4 * vtx::FLOATS];
    for (i, (x, y)) in [(8.0, 8.0), (24.0, 8.0), (24.0, 24.0), (8.0, 24.0)]
        .into_iter()
        .enumerate()
    {
        let o = &mut v[i * vtx::FLOATS..][..vtx::FLOATS];
        o[vtx::POSITION..][..4].copy_from_slice(&[
            ndc(x, ODSY_BOOT_WIDTH),
            ndc(y, ODSY_BOOT_HEIGHT),
            0.0,
            1.0,
        ]);
        o[vtx::COLOR..][..4].copy_from_slice(&[1.0, 1.0, 1.0, 0.25]);
        o[vtx::TEXCOORD..][..4].copy_from_slice(&[0.25, 0.25, 0.0, 1.0]);
    }
    let stipple = [0u32; 32];
    let mut d = LibDraw::new(GL_QUADS, 4, &v, &b.st, &stipple);
    b.board.lib_draw(&mut d);
    b.board.gpu_sync();
    pixel(&b.board, 0, 16, 16) & 0xff
}

/// Fragment alpha follows the GL texture environment table, which depends
/// on the texture format.
///
/// QEMU: rgba/texture-alpha.
#[test]
fn a_textured_fragments_alpha_follows_the_texture_environment_table() {
    const RGBA: u32 = 0x0300_0000;
    const RGB: u32 = 0x0280_0000;
    const LUMINANCE: u32 = 0x0100_0000;
    // (components, environment: 1 replace, 2 modulate, 3 decal, alpha)
    for (components, env, alpha) in [
        (RGBA, 1, 128),
        (RGBA, 2, 32),
        (RGBA, 3, 64),
        (RGB, 1, 64),
        (RGB, 3, 64),
        (LUMINANCE, 1, 64),
        (LUMINANCE, 2, 64),
    ] {
        assert_eq!(
            textured_alpha(components, env),
            alpha,
            "components {components:#x} environment {env}"
        );
    }
}

fn window() -> (Box<Board>, Arc<Ram>) {
    let (mut s, _, ram) = board_with_ram();
    s.win.clip_kept = 1;
    s.win.clip_rect[0] = ClipRect {
        x0: 0,
        y0: 0,
        x1: ODSY_WIDTH as i32 - 1,
        y1: ODSY_HEIGHT as i32 - 1,
    };
    (s, ram)
}

/// Word 4 of a GL read of the test screen; `first` and `second` pick the fields.
fn read_word4(class: u32, code: u32, first: u32, second: u32) -> u32 {
    (0x9263_3000 & !0x0ff0_0000) | first << 20 | second << 24 | class | code << 3
}

/// Byte `i` of a one-pixel DMA read at (x, y) of `bytes` bytes.
fn read_first_byte_at(
    s: &mut Board,
    ram: &Ram,
    (x, y): (u32, u32),
    word4: u32,
    bytes: u32,
    i: usize,
) -> u8 {
    let bpp_code = DMA_BPP.iter().position(|&b| b as u32 == bytes).unwrap() as u32;

    ram.put(PAGE_A, &[0xa5; 64]);
    put_table(
        ram,
        TABLE,
        [
            dma::SENTINEL_READ,
            y << 16 | x,
            0x50,
            1 << 16 | 1,
            word4,
            0,
            bytes,
            dma::TO_HOST | bpp_code << 16,
        ],
        &[pte(PAGE_A + 16, bytes)],
    );
    descriptor(s, TABLE);
    ram.get(PAGE_A + 16, 4)[i]
}

fn read_first_byte(s: &mut Board, ram: &Ram, at: (u32, u32), word4: u32, bytes: u32) -> u8 {
    read_first_byte_at(s, ram, at, word4, bytes, 0)
}

/// Word 4's field codes tell a depth or stencil read from a colour read of
/// the same class.
///
/// QEMU: readback/gl-depth-shared-rows.
#[test]
fn field_codes_tell_a_depth_or_stencil_read_from_a_colour_read() {
    let (mut s, ram) = window();
    pixel_write(&mut s, 30, 40, 0x7b);
    screen_field(&s, 1).store_word(30, 40, 0x7b);
    s.regs()
        .set_dispmode(0, ODSY_GAMMA_GROUP << dispmode::GROUP_SHIFT);

    // (class, code, bytes, first field code, second, first byte read)
    for (class, code, bytes, f1, f2, first) in [
        (2, 2, 4, 4, 0, 0x00u8),
        (0, 0, 1, 4, 0, 0x00),
        (2, 2, 4, 6, 2, 0x7b),
        (2, 2, 4, 2, 2, 0x7b),
    ] {
        let got = read_first_byte(
            &mut s,
            &ram,
            (30, 40),
            read_word4(class, code, f1, f2),
            bytes,
        );
        assert_eq!(got, first, "class {class} code {code} fields ({f1}, {f2})");
    }
}

/// The float with the depth word's exponent and top mantissa bits; its low
/// five mantissa bits are zero, so truncation returns the word exactly.
fn depth_float(word: u32) -> u32 {
    (((word >> 18) + 127 - 31) << 23) | (word & 0x3ffff) << 5
}

/// A depth clear stores the word its float names, with bits 31:24 clear.
///
/// QEMU: readback/gl-depth-word-encoding.
#[test]
fn a_depth_clear_stores_the_word_its_float_names() {
    let (mut s, _) = board("v12");
    probe_window(&mut s);
    set_reg(&mut s, reg::WRITE_MASK, &[0x01ff_ffff]);

    for (z, word) in [
        (DEPTH_2S, 0x00ff_fffe),
        (depth_float(0x7a1234), 0x007a_1234),
    ] {
        assert_eq!(depth_float(word), z, "the clear float for {word:#x}");
        clear(&mut s, [0, 0, 0], z);
        s.gpu_sync();
        assert_eq!(s.depth_load(300, 300), word, "clear float {z:#x}");
        assert_eq!(s.depth_load(257, 383), word);
    }
}

/// Stored depth words read back as 32-bit words and as their top 16 bits.
///
/// QEMU: readback/gl-depth-round-trip (read half only).
#[test]
fn stored_depth_words_read_back_in_order_in_both_forms() {
    let (mut s, ram) = window();
    s.regs()
        .set_dispmode(0, ODSY_GAMMA_GROUP << dispmode::GROUP_SHIFT);
    let words = [[0x0012_3456u32, 0x00ab_fffe], [0x0000_0001, 0x0080_cdef]];
    let before = s.depth_load(62, 62);

    for (r, row) in words.iter().enumerate() {
        for (c, &w) in row.iter().enumerate() {
            s.depth_store(60 + c as i64, 60 + r as i64, w);
        }
    }
    for (r, row) in words.iter().enumerate() {
        for (c, &w) in row.iter().enumerate() {
            let at = (60 + c as u32, 60 + r as u32);
            let bytes = (0..4)
                .map(|i| read_first_byte_at(&mut s, &ram, at, read_word4(2, 2, 4, 0), 4, i))
                .collect::<Vec<_>>();
            assert_eq!(
                bytes,
                w.to_be_bytes(),
                "32-bit read of ({}, {})",
                at.0,
                at.1
            );
            let short = (0..2)
                .map(|i| read_first_byte_at(&mut s, &ram, at, read_word4(1, 0, 4, 0), 2, i))
                .collect::<Vec<_>>();
            assert_eq!(
                short,
                ((w >> 8) as u16).to_be_bytes(),
                "16-bit read of ({}, {})",
                at.0,
                at.1
            );
        }
    }
    assert_eq!(
        s.depth_load(62, 62),
        before,
        "the pixel beside is untouched"
    );
}
