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
