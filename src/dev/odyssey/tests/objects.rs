//! Board-memory objects as texture levels: the model's object store keeps
//! what a game uploads, and texture setup finds a level's storage there.

use super::*;

/// TyrQuake on IRIX 6.5.30 keeps some 1,500 texture levels on the board at
/// once, each its own board-memory object, and samples the first ones it
/// loaded, the console font and the menu pictures, every frame. A table of
/// 1024 objects forgot the oldest and those draws went untextured, white.
#[test]
fn the_object_table_keeps_every_level_a_game_loads() {
    let (mut s, _, _) = board_with_ram();
    let first = 0x16b000;

    for i in 0..4096u32 {
        let o = s.object_select(first + i * 0x100, 16);
        assert!(s.object_rows(o, 1), "object {i}");
    }
    assert!(s.object_find(first) >= 0, "the first object loaded was forgotten");
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
