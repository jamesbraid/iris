//! Words the model reads by choice where the board's behaviour is unknown:
//! the mode register's cull fields and the kernel's address and data ports.

use crate::dev::odyssey::consts::mode;
use crate::dev::odyssey::gl_context::{GL_BACK, GL_FRONT, GL_FRONT_AND_BACK};
use crate::dev::odyssey::tests::{board, cmd};

/// Bit 8 with a face in bits 10:9 culls that face. Bit 8 with 00 is never
/// sent; the model culls nothing for it and does not keep an earlier face.
/// Bit 8 clear culls nothing whatever the face bits hold.
#[test]
fn cull_follows_the_word_alone() {
    let (mut s, _) = board("v12");
    let word = |enable: bool, faces: u32| (if enable { mode::CULL_FACE } else { 0 }) | (faces << mode::CULL_MODE_SHIFT);
    // (enable, faces, culls, face when it culls)
    let table = [
        (true, 1, true, GL_BACK),
        (true, 2, true, GL_FRONT),
        (true, 3, true, GL_FRONT_AND_BACK),
        (true, 0, false, 0),
        (false, 0, false, 0),
        (false, 3, false, 0),
        (true, 3, true, GL_FRONT_AND_BACK),
        (true, 0, false, 0),
    ];

    for (enable, faces, culls, face) in table {
        s.stream.pattern_mode = word(enable, faces);
        s.gl_apply_mode();
        let cur = &s.gl_ref().cur;
        assert_eq!(cur.cull_face, culls, "enable {enable}, faces {faces:02b}");
        if culls {
            assert_eq!(cur.cull_face_mode, face, "enable {enable}, faces {faces:02b}");
        }
    }
}

/// Words 0x740 and 0x741 are an address register and a data port that steps
/// the address. The 1024 words start at zero, accept writes, and an address
/// past the end is reported rather than wrapped. Any select value is
/// accepted; the old model refused all but two.
#[test]
fn the_kernel_port_pair_is_a_write_only_memory() {
    let (mut s, _) = board("v12");

    assert!(s.prim.local_mem.iter().all(|w| *w == 0));
    // (address, words sent, entries expected afterwards as (index, value))
    let table: [(u32, &[u32], &[(usize, u32)]); 4] = [
        (448, &[1, 2, 3], &[(448, 1), (449, 2), (450, 3), (451, 0)]),
        (491, &[7, 0, 0], &[(491, 7), (492, 0), (493, 0)]),
        (0x1ed, &[9], &[(0x1ed, 9)]),
        (1023, &[5], &[(1023, 5)]),
    ];

    for (addr, words, expect) in table {
        cmd(&mut s, 0x740);
        cmd(&mut s, addr);
        for w in words {
            cmd(&mut s, 0x741);
            cmd(&mut s, *w);
        }
        for (i, v) in expect {
            assert_eq!(s.prim.local_mem[*i], *v, "address {addr}, entry {i}");
        }
        assert_eq!(s.prim.restart_select, addr + words.len() as u32, "address {addr} steps once a word");
    }
    assert_eq!(s.diag.reported_cmd_count, 0, "everything above is in range");

    // Past the end: nothing stored, and reported.
    cmd(&mut s, 0x740);
    cmd(&mut s, 1024);
    cmd(&mut s, 0x741);
    cmd(&mut s, 0xdead);
    assert_eq!(s.diag.reported_cmd[0], 0x741);
}
