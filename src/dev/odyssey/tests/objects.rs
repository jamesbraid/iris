//! Board-memory objects as texture levels: the model's object store keeps
//! what a game uploads, and texture setup finds a level's storage there.

use crate::dev::odyssey::board::{Board, ODSY_OBJECTS};
use crate::dev::odyssey::consts::{ODSY_TEX_MODE_2D, ODSY_TEX_MODE_3D};
use crate::dev::odyssey::gl::{volume_slice_stride, TextureChain};
use crate::dev::odyssey::raster::GlTexImage;
use crate::dev::odyssey::tests::board_with_ram;

/// TyrQuake on IRIX 6.5.30 keeps some 1,500 texture levels on the board at
/// once, each its own board-memory object, and samples the first ones it
/// loaded, the console font and the menu pictures, every frame. A table of
/// 1024 objects forgot the oldest and those draws went untextured, white.
/// The table holds ODSY_OBJECTS, and only the one past that forgets.
#[test]
fn the_object_table_keeps_every_level_a_game_loads() {
    let (mut s, _, _) = board_with_ram();
    let base = |i: u32| 0x16b000 + i * 0x100;

    for i in 0..ODSY_OBJECTS {
        let o = s.object_select(base(i), 16);
        assert!(s.object_rows(o, 1), "object {i}");
    }
    assert!(s.object_find(base(0)).is_some(), "the first object loaded was forgotten");
    s.object_select(base(ODSY_OBJECTS), 16);
    assert!(s.object_find(base(0)).is_none(), "a full table forgets the oldest");
    assert!(s.object_find(base(1)).is_some() && s.object_find(base(ODSY_OBJECTS)).is_some());
}

/// The texture state of a plain 2D RGBA level, as chocolate-doom's SDL
/// renderer sends it for its 512x256 frame texture: clamp to edge on both
/// axes, nearest, replace, one level at `base` (sampler word 4 0x13001400,
/// level entry 0x41100020, replayed from a guest trace).
fn rgba_512x256(s: &mut Board, base: u32) {
    s.tex_mode = ODSY_TEX_MODE_2D;
    s.tex_level0 = 0;
    s.tex_levels[..3].copy_from_slice(&[base, 0x4110_0020, 0]);
    s.tex_sampler = [0x0800_0000 | (9 << 10) | (8 << 5), 0x0004_0000 | 512, 0x0004_0000 | 256, 1, 0x1300_1400, 0, 0];
    s.tex_env = [1, 0, 0, 0, 0];
    s.tex_lod = [0; 4];
}

/// glTexImage2D with no data and then glTexSubImage2D of a 320x200 frame,
/// SDL's way of streaming a texture: the object holds only the 200 rows the
/// subimage wrote of the level's 256. The level is still the client's
/// texture, so setup must take it, with the rows nobody wrote reading 0.
/// Before, setup called the level missing and drew the frame untextured,
/// a white window.
#[test]
fn a_level_written_only_in_part_is_still_the_texture() {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16b800;
    let obj = s.object_select(base, 512);

    for y in 0..200 {
        s.object_store(obj, 0, y, 0x1122_33ff);
    }
    rgba_512x256(&mut s, base);
    let mut image = GlTexImage::default();
    let mut chain = Box::<TextureChain>::default();

    assert!(s.texture_setup(&mut image, &mut chain));
    assert!(chain.levels[0].read_texel, "level 0 is missing");
    assert_eq!((chain.storage[0].width, chain.storage[0].height), (512, 256));
    assert_eq!(s.object_load(obj, 0, 199), 0x1122_33ff);
    assert_eq!(s.object_load(obj, 0, 255), 0, "a row the client never wrote");
}

/// The rows an object holds after texture setup under `word` (sampler word
/// 4), `sampler6` (word 6) and level entry word `entry1`, for the 512x256
/// level of an object `width` wide that holds 200 rows. The rest of the
/// state is rgba_512x256()'s.
fn rows_after_setup(width: u32, word: u32, sampler6: u32, entry1: u32) -> u32 {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16b800;
    let obj = s.object_select(base, width);

    assert!(s.object_rows(obj, 200));
    rgba_512x256(&mut s, base);
    s.tex_sampler[4] = word;
    s.tex_sampler[6] = sampler6;
    s.tex_levels[1] = entry1;
    if word & 0x3000 == 0 {
        // No clamp to edge, so no clamp mark in the size words.
        s.tex_sampler[1] = 512;
        s.tex_sampler[2] = 256;
    }
    let mut image = GlTexImage::default();
    let mut chain = Box::<TextureChain>::default();
    s.texture_setup(&mut image, &mut chain);
    s.obj_rows[obj as usize]
}

/// Only a level of the client's own row length grows: an object narrower
/// than the level, or one whose row length is not the level entry's, is
/// some other buffer and stays as it is.
#[test]
fn a_level_grows_only_into_an_object_of_its_row_length() {
    const ENTRY: u32 = 0x4110_0020;
    // A one-level entry may leave its row length 0 (no stride to check).
    const NO_STRIDE: u32 = 0x4110_0000;

    assert_eq!(rows_after_setup(512, 0x1300_1400, 0, ENTRY), 256, "the control");
    assert_eq!(rows_after_setup(512, 0x1300_1400, 0, NO_STRIDE), 256, "the control with no row length");
    assert_eq!(rows_after_setup(256, 0x1300_1400, 0, NO_STRIDE), 200, "narrower than the level");
    assert_eq!(rows_after_setup(1024, 0x1300_1400, 0, ENTRY), 200, "not the entry's row length");
}

/// A level with its border stored around it keeps its own checks: its
/// object needs rows past the level's height, so growing it to that height
/// would claim a level the border check then reads as written.
#[test]
fn a_bordered_level_does_not_grow() {
    // Word 4 bit 28 clear and word 6's top byte 1: the stored-border marks.
    assert_eq!(rows_after_setup(512, 0x0300_0000, 0x0100_0000, 0x4110_0020), 200);
}

/// The texture move draws only the rows it wrote, from a scratch object
/// sized to them; it must not grow that object to the power of two the
/// sampler names.
#[test]
fn a_texture_move_does_not_grow_its_object() {
    let (mut s, _, _) = board_with_ram();
    let base = 0x16b000;
    let obj = s.object_select(base, 512);

    assert!(s.object_rows(obj, 2));
    s.tex_mode = ODSY_TEX_MODE_2D;
    s.tex_level0 = 60;
    s.tex_levels[180..183].copy_from_slice(&[base, 0x8210_0020, 0]);
    s.tex_sampler = [(9 << 10) | (6 << 5), 512, 64, 0, 0x0364_0000, 0, 0];
    s.tex_env = [1, 0, 0, 0, 0];
    let mut image = GlTexImage::default();
    let mut chain = Box::<TextureChain>::default();
    assert!(s.texture_setup(&mut image, &mut chain));
    assert_eq!(s.obj_rows[obj as usize], 2);
}

/// Packing the store keeps every live object's texels and its place in
/// store order, closes the holes forgotten objects left, and puts them all
/// at the front. With a full table of 16384 objects, some moved to the end
/// by growing and a third forgotten, this is also the case where a packing
/// that searched the table for each object took a third of a second.
#[test]
fn packing_a_full_table_keeps_each_object_in_store_order() {
    let (mut s, _, _) = board_with_ram();
    let n = ODSY_OBJECTS as usize;

    for i in 0..n {
        let o = s.object_select(0x10_0000 + i as u32 * 0x100, 16);
        assert_eq!(o, i);
        s.object_store(o, 0, 0, i as u32);
    }
    for i in (0..n).step_by(7) {
        // Growth moves the object to the store's end.
        s.object_store(i, 0, 1, !(i as u32));
    }
    for i in (1..n).step_by(3) {
        s.object_forget(i);
    }
    let mut live: Vec<usize> = (0..n).filter(|&i| s.obj_base[i] != 0).collect();
    live.sort_by_key(|&i| s.obj_offset[i]);

    s.object_compact(None);
    let mut next = 0u32;
    for &i in &live {
        let o = i;
        assert_eq!(s.obj_offset[o], next, "object {i} is not packed in store order");
        next += s.obj_width[o] * s.obj_rows[o];
        assert_eq!(s.object_load(i, 0, 0), i as u32, "object {i}'s texel");
    }
    assert_eq!(s.obj_used, next);
}

/// A stored volume's slices keep their own check: every row and texel the
/// volume needs must be written. A slice written in part leaves the volume
/// unusable, and is not grown to look complete as a 2D level is. The
/// control, with every slice whole, shows the state is a usable volume.
#[test]
fn a_stored_volume_slice_written_in_part_does_not_grow() {
    for partial in [false, true] {
        let (mut s, _, _) = board_with_ram();
        let base = 0x10_0000;
        // A 4x4x4 RGBA volume with its border stored: each slice an object
        // 64 texels wide, the level 15 rows and columns in, plus the two
        // border slices.
        let (w, h, d) = (4u32, 4u32, 4u32);
        let stride = volume_slice_stride(h + 2) as u32;
        let mut rows = Vec::new();

        for z in 0..d + 2 {
            let o = s.object_select(base + z * stride, 64);
            let n = if partial && z == 2 { 18 } else { 15 + h + 2 };
            for y in 0..n {
                for x in 0..64 {
                    s.object_store(o, x, y, 0x1020_30ff);
                }
            }
            rows.push((o, n));
        }
        s.tex_mode = ODSY_TEX_MODE_3D;
        s.tex_level0 = 0;
        s.tex_levels[..3].copy_from_slice(&[base, 0x4110_0004, h + 2]);
        s.tex_sampler = [0x0800_0000 | (2 << 10) | (2 << 5) | 2, w, h, d, 0x0300_2a00, 0, 0x0100_0000];
        s.tex_env = [1, 0, 0, 0, 0];
        let mut image = GlTexImage::default();
        let mut chain = Box::<TextureChain>::default();
        let taken = s.texture_setup(&mut image, &mut chain) && chain.levels[0].read_texel_3d;

        assert_eq!(taken, !partial, "partial {partial}");
        for (o, n) in rows {
            assert_eq!(s.obj_rows[o as usize], n, "slice object {o}, partial {partial}");
        }
    }
}
